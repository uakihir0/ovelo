//! android.* builtins: Log, TextUtils, Uri, Intent, Context family,
//! SharedPreferences, Resources, Handler/Looper (inline), PackageManager.

use super::*;
use crate::value::{CanvasOp, HostState, Obj, PaintState, V};

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if class == "Landroid/util/Log;" {
        log_call(vm, name, args)
    } else if class == "Landroid/os/SystemProperties;" {
        // Private platform API — Compose reads debug flags through it
        // and registers a change listener; defaults + no-op keep the
        // AOSP surface intact (the callback only fires on setprop).
        match name {
            "get" => match args.len() {
                1 => Some(Some(jstr(vm, ""))),
                _ => Some(Some(args[1])),
            },
            "getInt" => Some(Some(args.get(1).copied().unwrap_or(V::I32(0)))),
            "getLong" => Some(Some(args.get(1).copied().unwrap_or(V::I64(0)))),
            "getBoolean" => Some(Some(args.get(1).copied().unwrap_or(V::I32(0)))),
            "set" | "addChangeCallback" | "removeChangeCallback" | "reportSyspropChanged" => {
                Some(None)
            }
            _ => None,
        }
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
    } else if class == "Landroid/net/ConnectivityManager;"
        || class == "Landroid/net/NetworkCapabilities;"
        || class == "Landroid/net/NetworkInfo;"
        || class == "Landroid/net/Network;"
    {
        network_call(vm, class, name, args)
    } else if desc_is_a(class, "Landroid/net/Uri;") || class == "Landroid/net/Uri$Builder;" {
        uri_call(vm, name, args)?
    } else if desc_is_a(class, "Landroid/content/Intent;") {
        intent_call(vm, name, sig, args)?
    } else if class == "Landroid/content/ComponentName;" {
        component_call(vm, name, args)?
    } else if class == "Landroid/app/ActivityManager;" {
        // Report a normal modern-device memory budget. Coil derives
        // its memory-cache byte capacity from these values; the
        // placeholder default of zero makes `LruCache` throw
        // `maxSize <= 0` before image fetching even starts.
        match name {
            "getMemoryClass" => Some(Some(V::I32(256))),
            "getLargeMemoryClass" => Some(Some(V::I32(384))),
            "isLowRamDevice" => Some(Some(V::I32(0))),
            _ => None,
        }
    } else if class == "Landroid/os/UserManager;" {
        match name {
            // Ordinary interactive users are unlocked; WorkManager
            // distinguishes direct-boot from normal startup with this.
            "isUserUnlocked" => Some(Some(V::I32(1))),
            "isSystemUser" | "isPrimaryUser" => Some(Some(V::I32(1))),
            "isGuestUser" | "isRestrictedProfile" | "isUserAGoat" => Some(Some(V::I32(0))),
            _ => None,
        }
    } else if class == "Landroid/view/inputmethod/InputMethodManager;" {
        match name {
            "showSoftInput" => {
                let view = match args.get(1) {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                };
                Some(Some(V::I32(
                    view::offer_input_connection(vm, view).map_err(crate::E::Vm)? as i32,
                )))
            }
            "restartInput" => {
                let view = match args.get(1) {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                };
                let _ = view::offer_input_connection(vm, view).map_err(crate::E::Vm)?;
                Some(None)
            }
            "isActive" => {
                let active = match args.get(1) {
                    Some(V::Ref(h)) => view::has_input_connection(vm, *h),
                    _ => false,
                };
                Some(Some(V::I32(active as i32)))
            }
            "hideSoftInputFromWindow"
            | "hideSoftInputFromInputMethod"
            | "toggleSoftInput"
            | "showInputMethodPicker"
            | "showInputMethodPickerFromSystem" => Some(None),
            _ => None,
        }
    } else if class.starts_with("Landroid/app/job/") {
        job_call(vm, class, name, args)?
    } else if class == "Landroid/provider/DocumentsContract;" {
        // SAF URI builders only mint content:// strings — like Uri.parse.
        match name {
            "buildTreeDocumentUri"
            | "buildDocumentUri"
            | "buildDocumentUriUsingTree"
            | "buildChildDocumentsUri"
            | "buildChildDocumentsUriUsingTree" => {
                let a0 = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                let id = sv(vm, args.get(1).copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                // DocIds are percent-encoded into the URI segment
                // (`primary:Music/Recordings` → `primary%3AMusic%2FRecordings`)
                // so embedded `/` never forges a `/document/` or
                // `/children` boundary — `DocumentFile` calls
                // `getDocumentId` which decodes it back.
                let id = db::pct_enc(&id);
                let s = if name.contains("UsingTree") {
                    // a[0] is the tree Uri itself — AOSP appends
                    // `/document/{id}` (and `/children` for the
                    // children listing), preserving /tree/{treeId}.
                    if name.starts_with("buildChild") {
                        format!("{a0}/document/{id}/children")
                    } else {
                        format!("{a0}/document/{id}")
                    }
                } else if name.starts_with("buildChild") {
                    format!("content://{a0}/document/{id}/children")
                } else {
                    let seg = if name == "buildTreeDocumentUri" {
                        "tree"
                    } else {
                        "document"
                    };
                    format!("content://{a0}/{seg}/{id}")
                };
                Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(s))))
            }
            "getDocumentId" | "getTreeDocumentId" => {
                // A docId embeds `/` (`primary:Music/Recordings`): the
                // document id is the whole remainder after the LAST
                // `/document/`; the tree id is the remainder after the
                // LAST `/tree/` up to the next `/document/` segment.
                let u = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                let id = if name == "getTreeDocumentId" {
                    u.rfind("/tree/")
                        .map(|off| {
                            let rest = &u[off + "/tree/".len()..];
                            rest.find("/document/").map_or(rest, |e| &rest[..e])
                        })
                        .unwrap_or_default()
                } else {
                    ["/document/", "/tree/"]
                        .iter()
                        .find_map(|pat| u.rfind(pat).map(|off| &u[off + pat.len()..]))
                        .unwrap_or_default()
                };
                let id = db::pct_dec(id);
                Some(Some(if u.is_empty() {
                    V::Ref(0)
                } else {
                    jstr(vm, &id)
                }))
            }
            "isTreeUri" | "isDocumentUri" => {
                let u = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                let yes = if name == "isTreeUri" {
                    u.contains("/tree/")
                } else {
                    u.contains("/document/")
                };
                Some(Some(V::I32(yes as i32)))
            }
            _ => None,
        }
    } else if class == "Landroid/content/IntentFilter;" {
        match name {
            "addAction" | "addCategory" | "addDataScheme" | "addDataType" | "addDataAuthority"
            | "addDataPath" | "setPriority" => Some(None),
            "matchAction" | "hasAction" | "hasCategory" => Some(Some(V::I32(1))),
            "match" => Some(Some(V::I32(0))),
            "countActions" | "countCategories" => Some(Some(V::I32(0))),
            "getAction" => Some(Some(V::Ref(0))),
            "create" => Some(Some(host(vm, class, HostState::Empty))),
            "describeContents" | "writeToParcel" => Some(None),
            _ => None,
        }
    } else if class == "Landroid/database/Observable;" {
        // androidx `AdapterDataObservable` et al. extend this framework
        // class that ships no dex — without the ctor `mObservers` stays
        // null and every notify NPEs inside `add`.
        match name {
            "<init>" => {
                let list = host(vm, "Ljava/util/ArrayList;", HostState::List(vec![]));
                if let V::Ref(this) = args[0] {
                    field_set(vm, this, class, "mObservers", list);
                }
                Some(None)
            }
            "registerObserver" => {
                if let V::Ref(this) = args[0] {
                    let obs = args.get(1).copied().unwrap_or(V::Ref(0));
                    if let Some(V::Ref(lh)) = field_get(vm, this, class, "mObservers") {
                        if let Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) = vm.heap.get_mut(lh)
                        {
                            l.push(obs);
                        }
                    }
                }
                Some(None)
            }
            "unregisterObserver" => {
                if let V::Ref(this) = args[0] {
                    let obs = args.get(1).copied().unwrap_or(V::Ref(0));
                    if let Some(V::Ref(lh)) = field_get(vm, this, class, "mObservers") {
                        if let Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) = vm.heap.get_mut(lh)
                        {
                            l.retain(|o| *o != obs);
                        }
                    }
                }
                Some(None)
            }
            "unregisterAll" => {
                if let V::Ref(this) = args[0] {
                    if let Some(V::Ref(lh)) = field_get(vm, this, class, "mObservers") {
                        if let Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) = vm.heap.get_mut(lh)
                        {
                            l.clear();
                        }
                    }
                }
                Some(None)
            }
            _ => None,
        }
    } else if class == "Landroid/preference/PreferenceManager;" {
        // Static factory — map-backed SharedPreferences, interned
        // per file so writes are visible to later readers.
        match name {
            "getDefaultSharedPreferences" => {
                let name = format!(
                    "{}_preferences",
                    vm.guest_package.as_deref().unwrap_or("app")
                );
                Some(Some(prefs_file(vm, &name)))
            }
            _ => None,
        }
    } else if class == "Landroid/content/SharedPreferences;"
        || class.ends_with("SharedPreferences;")
        || class == "Landroid/content/SharedPreferences$Editor;"
    {
        prefs_call(vm, name, args)?
    } else if desc_is_a(class, "Landroid/app/Dialog;") {
        dialog_call(vm, name, args)?
    } else if desc_is_a(class, "Landroid/content/Context;")
        || class.ends_with("Context;")
        || class.ends_with("Activity;")
        || class.ends_with("Application;")
        || class.ends_with("Service;")
        // Dex-defined Context subclasses (custom Application/Service
        // classes like `PFSudoku extends MultiDexApplication`) — walk
        // the dex hierarchy to the framework Context.
        || vm.assignable_desc(class, "Landroid/content/Context;")
    {
        context_call(vm, class, name, sig, args)?
    } else if class == "Landroid/content/pm/PackageManager;" || class.ends_with("PackageManager;") {
        pm_call(vm, name, args)?
    } else if class == "Landroid/content/pm/PackageItemInfo;"
        || matches!(
            class,
            "Landroid/content/pm/ResolveInfo;"
                | "Landroid/content/pm/ActivityInfo;"
                | "Landroid/content/pm/ServiceInfo;"
                | "Landroid/content/pm/ProviderInfo;"
                | "Landroid/content/pm/ApplicationInfo;"
                | "Landroid/content/pm/PackageInfo;"
                | "Landroid/appwidget/AppWidgetProviderInfo;"
        )
    {
        item_info_call(vm, name, args)?
    } else if class == "Landroid/content/ContentResolver;" || class.ends_with("ContentResolver;") {
        match name {
            "query" | "rawQuery" => {
                let uri = args
                    .iter()
                    .skip(1)
                    .find_map(|a| sv(vm, *a))
                    .unwrap_or_default();
                // query(uri, projection, selection, selectionArgs,
                // sortOrder): honor the projection — positional reads
                // (getString(0)) must return the requested column.
                let projection: Vec<String> = match args.get(2) {
                    Some(V::Ref(ph)) => match vm.heap.get(*ph) {
                        Some(Obj::Array { data, .. }) => {
                            data.iter().filter_map(|v| sv(vm, *v)).collect()
                        }
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                if uri.starts_with("content://media/") {
                    Some(Some(db::media_cursor(vm, &uri, &projection)))
                } else if uri.contains("externalstorage.documents") {
                    Some(Some(db::saf_cursor(vm, &uri, &projection)))
                } else {
                    Some(Some(host(
                        vm,
                        "Landroid/database/Cursor;",
                        HostState::Empty,
                    )))
                }
            }
            "openFileDescriptor" | "openAssetFileDescriptor" | "openTypedAssetFileDescriptor" => {
                // Glide's local-uri fetcher reads the pixels through a
                // parcelled descriptor — hand it the file's real bytes.
                let uri = sv(vm, args.get(1).copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                let bytes = media_uri_path(vm, &uri)
                    .and_then(|p| std::fs::read(io::fspath(vm, &p)).ok())
                    .unwrap_or_default();
                if bytes.is_empty() {
                    Some(Some(V::Ref(0)))
                } else {
                    let d = if name == "openFileDescriptor" {
                        "Landroid/os/ParcelFileDescriptor;"
                    } else {
                        "Landroid/content/res/AssetFileDescriptor;"
                    };
                    Some(Some(host(
                        vm,
                        d,
                        HostState::Bytes(std::rc::Rc::new(bytes), 0),
                    )))
                }
            }
            // SAF permission list: no persisted grants — guests iterate it.
            "getPersistedUriPermissions" => {
                Some(Some(host(vm, "Ljava/util/List;", HostState::List(vec![]))))
            }
            "insert"
            | "call"
            | "openFile"
            | "getType"
            | "getStreamTypes"
            | "takePersistableUriPermission"
            | "loadThumbnail" => Some(Some(V::Ref(0))),
            "delete"
            | "update"
            | "bulkInsert"
            | "getOutgoingPersistedUriPermissions"
            | "getSyncStatus"
            | "startSync"
            | "requestSync" => Some(Some(V::I32(0))),
            "getSyncAdapterTypes" => {
                Some(Some(arr(vm, "Landroid/content/SyncAdapterType;", vec![])))
            }
            "openInputStream" => {
                let uri = sv(vm, args.get(1).copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                let bytes = media_uri_path(vm, &uri)
                    .and_then(|p| std::fs::read(io::fspath(vm, &p)).ok())
                    .unwrap_or_default();
                Some(Some(host(
                    vm,
                    "Ljava/io/FileInputStream;",
                    HostState::Bytes(std::rc::Rc::new(bytes), 0),
                )))
            }
            "openOutputStream" => {
                let uri = sv(vm, args.get(1).copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                // SAF documents resolve to real files under
                // external_root so `canWrite()` documents actually
                // persist bytes. mode: "w" truncate (default),
                // "wa" append, "rw"/"rwt" read+write.
                let mode = sv(vm, args.get(2).copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                if uri.contains("externalstorage.documents") {
                    let (_id, rel) = db::saf_doc_rel(&uri);
                    let path = vm.external_root.join(&rel);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // Modes: "w" truncate, "wa" append, "rw" r/w no
                    // truncate, "rwt" r/w + truncate.
                    let f = std::fs::OpenOptions::new()
                        .write(true)
                        .read(mode.starts_with("rw"))
                        .create(true)
                        .truncate(mode != "wa" && mode != "rw")
                        .append(mode == "wa")
                        .open(&path)
                        .ok();
                    return Ok(Some(match f {
                        Some(f) => Some(host(vm, "Ljava/io/FileOutputStream;", HostState::File(f))),
                        None => Some(V::Ref(0)),
                    }));
                }
                Some(Some(host(
                    vm,
                    "Ljava/io/FileOutputStream;",
                    HostState::Empty,
                )))
            }
            "isSyncActive"
            | "getSyncAutomatically"
            | "getMasterSyncAutomatically"
            | "getIsSyncable"
            | "validateSyncExtrasBundle"
            | "canPerformSync" => Some(Some(V::I32(0))),
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
    } else if class == "Landroid/content/ContentProvider;"
        || vm.assignable_desc(class, "Landroid/content/ContentProvider;")
    {
        // Guest providers (androidx.startup.InitializationProvider & co.)
        // are dex classes; assignable_desc walks the dex chain to the
        // framework base. `attachInfo` stashes the Context for
        // `getContext` — the real ContentProvider path.
        match name {
            "attachInfo" => {
                if let V::Ref(this) = args[0] {
                    view::set_dialog_ctx(vm, this, args.get(1).copied().unwrap_or(V::Ref(0)));
                }
                Some(None)
            }
            "getContext" => {
                if let V::Ref(this) = args[0] {
                    Some(Some(view::dialog_ctx(vm, this)))
                } else {
                    Some(Some(V::Ref(0)))
                }
            }
            "getContentResolver" => Some(Some(host(
                vm,
                "Landroid/content/ContentResolver;",
                HostState::Empty,
            ))),
            _ => None,
        }
    } else if class == "Landroid/content/res/Resources;" || class.ends_with("Resources;") {
        resources_call(vm, name, args)?
    } else if class == "Landroid/util/Xml;" {
        match name {
            // The parser IS its own AttributeSet — hand back the same
            // handle so attribute reads follow the cursor.
            "asAttributeSet" => Some(Some(args.first().copied().unwrap_or(V::Ref(0)))),
            "newPullParser" => Some(Some(host(
                vm,
                "Landroid/content/res/XmlResourceParser;",
                HostState::Empty,
            ))),
            _ => None,
        }
    } else if class == "Landroid/content/res/XmlResourceParser;"
        || class == "Lorg/xmlpull/v1/XmlPullParser;"
        || class == "Lorg/xmlpull/v1/XmlPullParserException;"
    {
        xml::pull_call(vm, class, name, sig, args)?
    } else if class == "Landroid/util/DisplayMetrics;"
        || class == "Landroid/content/res/Configuration;"
        || class == "Landroid/content/res/TypedArray;"
        || class == "Landroid/util/TypedValue;"
        || class == "Landroid/content/res/ColorStateList;"
        || class == "Landroid/content/res/AssetManager;"
        || class == "Landroid/content/res/Resources$Theme;"
        || class == "Landroid/os/LocaleList;"
    {
        res_misc(vm, class, name, args)?
    } else if class == "Landroid/os/Handler;" || class.ends_with("Handler;") {
        handler_call(vm, name, args)?
    } else if class == "Landroid/os/Looper;" {
        looper_call(vm, name, args)?
    } else if class == "Landroid/os/Message;" || class == "Landroid/os/Messenger;" {
        match name {
            "obtain" | "obtainMessage" => {
                // obtain(Handler[, what|Runnable|obj ...]): first Ref is
                // the target handler; a second Ref is the callback
                // Runnable or the `obj` payload.
                let m = host(vm, "Landroid/os/Message;", HostState::Empty);
                if let V::Ref(mh) = m {
                    let refs: Vec<u32> = args
                        .iter()
                        .filter_map(|a| if let V::Ref(h) = a { Some(*h) } else { None })
                        .collect();
                    if let Some(&t) = refs.first() {
                        pseudo_set(vm, mh, P_MSG_TARGET, V::Ref(t));
                    }
                    if let Some(&o) = refs.get(1) {
                        if args.len() == 2 {
                            pseudo_set(vm, mh, P_MSG_CALLBACK, V::Ref(o));
                        } else {
                            pseudo_set(vm, mh, P_MSG_OBJ, V::Ref(o));
                        }
                    }
                    // Int args after the target are what/arg1/arg2.
                    let after_target = args
                        .iter()
                        .position(|a| matches!(a, V::Ref(_)))
                        .map(|i| &args[i + 1..])
                        .unwrap_or(&[]);
                    let mut ints = after_target
                        .iter()
                        .filter(|a| matches!(a, V::I32(_) | V::I64(_)));
                    if let Some(&w) = ints.next() {
                        pseudo_set(vm, mh, P_MSG_WHAT, w);
                    }
                    if let Some(&a1) = ints.next() {
                        pseudo_set(vm, mh, P_MSG_ARG1, a1);
                    }
                    if let Some(&a2) = ints.next() {
                        pseudo_set(vm, mh, P_MSG_ARG2, a2);
                    }
                }
                Some(Some(m))
            }
            "sendToTarget" => {
                if let Some(&V::Ref(mh)) = args.first() {
                    vm.enqueue_runnable(mh, true);
                }
                Some(Some(V::I32(1)))
            }
            "getTarget" => Some(Some(match args.first() {
                Some(&V::Ref(mh)) => pseudo_get(vm, mh, P_MSG_TARGET).unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            })),
            "getCallback" => Some(Some(match args.first() {
                Some(&V::Ref(mh)) => pseudo_get(vm, mh, P_MSG_CALLBACK).unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            })),
            "obj" | "getObj" => Some(Some(match args.first() {
                Some(&V::Ref(mh)) => pseudo_get(vm, mh, P_MSG_OBJ).unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            })),
            "setTarget" => {
                if let (Some(&V::Ref(mh)), Some(&t)) = (args.first(), args.get(1)) {
                    pseudo_set(vm, mh, P_MSG_TARGET, t);
                }
                Some(None)
            }
            "recycle" | "send" | "sendMessage" | "sendMessageDelayed" | "sendEmptyMessage" => {
                Some(if name.starts_with("send") {
                    Some(V::I32(1))
                } else {
                    None
                })
            }
            "getData" | "peekData" | "getMessenger" => Some(Some(V::Ref(0))),
            "getWhen" | "getSendingUid" => Some(Some(V::I64(0))),
            "setData" | "copyFrom" => Some(None),
            _ => None,
        }
    } else if class == "Landroid/os/SystemClock;" {
        match name {
            "uptimeMillis" | "elapsedRealtime" | "currentThreadTimeMillis" => {
                Some(Some(V::I64(now_ms())))
            }
            "elapsedRealtimeNanos" | "nanoTime" => Some(Some(V::I64(now_ms() * 1_000_000))),
            "sleep" | "setCurrentTimeMillis" => Some(None),
            _ => None,
        }
    } else if class == "Landroid/view/animation/AnimationUtils;" {
        match name {
            "currentAnimationTimeMillis" => Some(Some(V::I64(now_ms()))),
            _ => None,
        }
    } else if class == "Landroid/os/Process;" {
        match name {
            "myPid" | "myTid" | "myUid" | "myUserHandle" => {
                Some(Some(V::I32(if name.contains("Uid") { 1000 } else { 1 })))
            }
            "getThreadPriority" => Some(Some(V::I32(0))),
            "setThreadPriority" | "killProcess" | "sendSignal" => Some(None),
            "getElapsedCpuTime"
            | "getStartElapsedRealtime"
            | "getStartRequestedElapsedRealtime"
            | "getStartUptimeMillis"
            | "getFreeMemory"
            | "getTotalMemory" => Some(Some(V::I64(0))),
            "is64Bit" | "isApplicationUid" | "supportsProcesses" | "isSdkSandbox"
            | "isIsolated" => Some(Some(V::I32((name == "is64Bit") as i32))),
            _ => None,
        }
    } else if class.starts_with("Landroid/os/StrictMode$") {
        // ThreadPolicy/VmPolicy and their Builders: builder mutators
        // return the builder; build() mints the matching policy.
        match name {
            "build" => Some(Some(host(
                vm,
                if class.contains("VmPolicy") {
                    "Landroid/os/StrictMode$VmPolicy;"
                } else {
                    "Landroid/os/StrictMode$ThreadPolicy;"
                },
                HostState::Empty,
            ))),
            "toString" | "hashCode" => {
                if name == "hashCode" {
                    Some(Some(V::I32(0)))
                } else {
                    Some(Some(jstr(vm, class)))
                }
            }
            "<init>" => Some(None),
            _ => Some(Some(args[0])),
        }
    } else if class == "Landroid/os/Trace;"
        || class == "Landroid/os/StrictMode;"
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
            "allowThreadDiskWrites" | "allowThreadDiskReads" | "getThreadPolicy" => Some(Some(
                host(vm, "Landroid/os/StrictMode$ThreadPolicy;", HostState::Empty),
            )),
            "getVmPolicy" | "build" => Some(Some(host(
                vm,
                "Landroid/os/StrictMode$VmPolicy;",
                HostState::Empty,
            ))),
            "detectAll"
            | "penaltyLog"
            | "penaltyDeath"
            | "setVmPolicy"
            | "setThreadPolicy"
            | "noteSlowCall"
            | "noteUntaggedSocket"
            | "noteDiskRead"
            | "noteDiskWrite"
            | "getInstanceCount"
            | "getGlobalAllocSize"
            | "getGlobalAllocCount"
            | "getNativeHeapAllocatedSize"
            | "getNativeHeapSize"
            | "getNativeHeapFreeSize"
            | "getPss"
            | "getMemoryInfo" => {
                if name.ends_with("Size")
                    || name.ends_with("Pss")
                    || name.ends_with("Count")
                    || name.ends_with("Info")
                {
                    Some(Some(V::I64(0)))
                } else {
                    Some(None)
                }
            }
            "dumpHeap" | "waitingForDebugger" | "isDebuggerConnected" => Some(Some(V::I32(0))),
            _ => Some(None),
        }
    } else if class == "Landroid/os/Environment;" {
        match name {
            // The guest sandbox's shared storage: real File I/O maps
            // absolute guest paths under vm.guest_root, so report the
            // canonical emulated path.
            "getExternalStorageDirectory" | "getStorageDirectory" => Some(Some(host(
                vm,
                "Ljava/io/File;",
                HostState::Path("/storage/emulated/0".into()),
            ))),
            "getExternalStoragePublicDirectory" => {
                // e.g. Environment.DIRECTORY_DCIM → /storage/emulated/0/DCIM.
                // Static method: the directory name is the first arg.
                let dir = sv(vm, *args.first().unwrap_or(&V::Ref(0))).unwrap_or_default();
                Some(Some(host(
                    vm,
                    "Ljava/io/File;",
                    HostState::Path(format!("/storage/emulated/0/{dir}")),
                )))
            }
            "getExternalStorageState" | "getStorageState" | "getExternalStorageStateLegacy" => {
                Some(Some(jstr(vm, "mounted")))
            }
            "getDataDirectory" => Some(Some(host(
                vm,
                "Ljava/io/File;",
                HostState::Path("/data".into()),
            ))),
            "getRootDirectory" => Some(Some(host(
                vm,
                "Ljava/io/File;",
                HostState::Path("/system".into()),
            ))),
            "getDownloadCacheDirectory" => Some(Some(host(
                vm,
                "Ljava/io/File;",
                HostState::Path("/cache".into()),
            ))),
            "isExternalStorageRemovable"
            | "isExternalStorageEmulated"
            | "isExternalStorageManager" => Some(Some(V::I32(1))),
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
    } else if class.ends_with("ParcelFileDescriptor;")
        || class == "Landroid/content/res/AssetFileDescriptor;"
    {
        match name {
            "getFileDescriptor" => {
                let (b, p) = match args.first().copied() {
                    Some(V::Ref(h)) => match vm.heap.get(h) {
                        Some(Obj::Host {
                            state: HostState::Bytes(b, p),
                            ..
                        }) => (b.clone(), *p),
                        _ => (std::rc::Rc::new(vec![]), 0),
                    },
                    _ => (std::rc::Rc::new(vec![]), 0),
                };
                Some(Some(host(
                    vm,
                    "Ljava/io/FileDescriptor;",
                    HostState::Bytes(b, p),
                )))
            }
            "getStatSize" | "getLength" | "getDeclaredLength" => {
                let n = match args.first().copied() {
                    Some(V::Ref(h)) => match vm.heap.get(h) {
                        Some(Obj::Host {
                            state: HostState::Bytes(b, _),
                            ..
                        }) => b.len() as i64,
                        _ => 0,
                    },
                    _ => 0,
                };
                Some(Some(V::I64(n)))
            }
            "getParcelFileDescriptor" => {
                // AssetFileDescriptor carries an inner ParcelFileDescriptor
                // over the same bytes — minting a correctly-typed host keeps
                // later `check-cast ParcelFileDescriptor` honest.
                let st = match args.first().copied() {
                    Some(V::Ref(h)) => match vm.heap.get(h) {
                        Some(Obj::Host {
                            state: HostState::Bytes(b, p),
                            ..
                        }) => HostState::Bytes(b.clone(), *p),
                        _ => HostState::Empty,
                    },
                    _ => HostState::Empty,
                };
                Some(Some(host(vm, "Landroid/os/ParcelFileDescriptor;", st)))
            }
            "close" | "detachFd" | "dup" | "canDetectErrors" => match name {
                "canDetectErrors" => Some(Some(V::I32(0))),
                "dup" => Some(Some(args[0])),
                _ => Some(None),
            },
            _ => None,
        }
    } else if class == "Landroid/os/Binder;"
        || class == "Landroid/os/Parcel;"
        || class == "Landroid/os/IBinder;"
    {
        match name {
            "getCallingUid" | "getCallingPid" | "getCallingUserHandle" => Some(Some(V::I32(1000))),
            "clearCallingIdentity" | "restoreCallingIdentity" | "obtain" => Some(Some(V::I64(0))),
            "flushPendingCommands" | "isBinderAlive" | "pingBinder" => Some(Some(V::I32(1))),
            "writeInt" | "writeLong" | "writeString" | "writeByte" | "writeFloat"
            | "writeDouble" | "writeByteArray" | "writeValue" | "writeStrongBinder"
            | "writeParcelable" | "writeBundle" | "writeStringArray" | "writeIntArray"
            | "readInt" | "readLong" | "readString" | "readByte" | "readFloat" | "readDouble"
            | "readByteArray" | "readValue" | "readStrongBinder" | "readParcelable"
            | "readBundle" | "recycle" | "dataSize" | "dataPosition" | "setDataPosition"
            | "marshall" | "unmarshall" | "transact" | "linkToDeath" | "unlinkToDeath" => {
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
            "getInterfaceDescriptor" | "getInterfaceDescriptor0" => Some(Some(jstr(vm, ""))),
            _ => None,
        }
    } else if class == "Landroid/content/ClipData;" || class == "Landroid/content/ClipboardManager;"
    {
        match name {
            "newPlainText" | "newUri" | "newIntent" => Some(Some(host(
                vm,
                "Landroid/content/ClipData;",
                HostState::Empty,
            ))),
            "setPrimaryClip" | "setText" | "clearPrimaryClip" => Some(None),
            "getPrimaryClip" | "getText" | "getPrimaryClipDescription" => Some(Some(V::Ref(0))),
            "hasPrimaryClip" | "hasText" => Some(Some(V::I32(0))),
            "getItemCount" => Some(Some(V::I32(0))),
            "getItemAt" => Some(Some(V::Ref(0))),
            _ => None,
        }
    } else if class == "Landroid/content/ContentUris;" {
        match name {
            // withAppendedId(Uri, long) — players build per-track URIs
            // like content://media/external/audio/media/<id> with it.
            "withAppendedId" => {
                let base = sv(vm, *args.first().unwrap_or(&V::Ref(0))).unwrap_or_default();
                let id = i64v(*args.get(1).unwrap_or(&V::I64(0)));
                let sep = if base.ends_with('/') { "" } else { "/" };
                Some(Some(host(
                    vm,
                    "Landroid/net/Uri;",
                    HostState::StrBuf(format!("{base}{sep}{id}")),
                )))
            }
            // parseId(Uri) — AOSP reads the LAST path segment: -1 when
            // the URI has no segment, NumberFormatException when the
            // segment exists but isn't numeric.
            "parseId" => {
                let uri = sv(vm, *args.first().unwrap_or(&V::Ref(0))).unwrap_or_default();
                match uri.rsplit('/').next().filter(|t| !t.is_empty()) {
                    None => Some(Some(V::I64(-1))),
                    Some(tail) => match tail.parse::<i64>() {
                        Ok(id) => Some(Some(V::I64(id))),
                        Err(_) => {
                            return Err(vm.throw_builtin("Ljava/lang/NumberFormatException;", tail));
                        }
                    },
                }
            }
            _ => Some(Some(V::Ref(0))),
        }
    } else if class == "Landroid/app/NotificationManager;"
        || class == "Landroid/app/NotificationManagerCompat;"
    {
        match name {
            // notify/cancel are presentation-only — succeed silently.
            "cancel" | "cancelAll" | "notify" => Some(None),
            "areNotificationsEnabled" => Some(Some(V::I32(1))),
            _ => Some(Some(V::Ref(0))),
        }
    } else if class == "Landroid/database/Cursor;"
        || class.ends_with("Cursor;")
        || class.starts_with("Landroid/database/")
    {
        super::db::db_call(vm, name, args)?
    } else if class.starts_with("Landroid/provider/") {
        match name {
            // `MediaStore$Files.getContentUri("external")` →
            // content://media/external/file — same segment logic as
            // the *_CONTENT_URI fields (no extra suffix).
            "getContentUri" => {
                let media = if class == "Landroid/provider/MediaStore$Files;" {
                    "file".to_string()
                } else {
                    class
                        .trim_start_matches("Landroid/provider/MediaStore$")
                        .trim_end_matches(';')
                        .split('$')
                        .collect::<Vec<_>>()
                        .join("/")
                        .to_lowercase()
                };
                let vol = sv(vm, *args.first().unwrap_or(&V::Ref(0)))
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "external".into());
                Some(Some(host(
                    vm,
                    "Landroid/net/Uri;",
                    HostState::StrBuf(format!("content://media/{vol}/{media}")),
                )))
            }
            "getString" | "getInt" | "getLong" | "getFloat" => {
                Some(Some(args.get(2).copied().unwrap_or(V::I32(0))))
            }
            "putString" | "putInt" | "putLong" | "putFloat" => Some(Some(V::I32(1))),
            // `Telephony$Sms.getDefaultSmsPackage` — report ourselves as
            // the default SMS app so SMS apps don't bail to a setup UI.
            "getDefaultSmsPackage" if class.contains("Telephony") => {
                let pkg = vm.guest_package.clone().unwrap_or_else(|| "app".into());
                Some(Some(jstr(vm, &pkg)))
            }
            _ => Some(Some(V::Ref(0))),
        }
    } else if class == "Landroid/util/LruCache;" {
        match name {
            "get" => Some(Some(
                map_get(
                    vm,
                    match args[0] {
                        V::Ref(h) => h,
                        _ => 0,
                    },
                    args[1],
                )
                .unwrap_or(V::Ref(0)),
            )),
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
        || class == "Landroid/util/Size;"
        || class == "Landroid/util/SizeF;"
        || class == "Landroid/util/AttributeSet;"
    {
        graphics_call(vm, class, name, sig, args)?
    } else if class == "Landroid/animation/AnimatorInflater;" {
        match name {
            // Material checks `animator instanceof ObjectAnimator` and
            // throws otherwise — return an ObjectAnimator-typed stub.
            "loadAnimator" => Some(Some(host(
                vm,
                "Landroid/animation/ObjectAnimator;",
                HostState::Empty,
            ))),
            "loadStateListAnimator" => Some(Some(host(
                vm,
                "Landroid/animation/StateListAnimator;",
                HostState::Empty,
            ))),
            _ => Some(None),
        }
    } else if class == "Landroid/app/role/RoleManager;" {
        // SMS-role gate: fossify messages finishes when ROLE_SMS is
        // unavailable or not held — report both so the app proceeds as
        // the default SMS handler.
        match name {
            "isRoleAvailable" | "isRoleHeld" => Some(Some(V::I32(1))),
            "createRequestRoleIntent" => {
                Some(Some(host(vm, "Landroid/content/Intent;", HostState::Empty)))
            }
            _ => None,
        }
    } else if class == "Landroid/view/View$MeasureSpec;" {
        measurespec_call(vm, name, args)
    } else if class == "Landroid/view/ViewConfiguration;" || class == "Landroid/view/Gravity;" {
        viewconfig_call(vm, class, name, args)?
    } else if view::is_viewish(vm, class, args) {
        view::call(vm, class, name, sig, args)?
    } else {
        return Ok(None);
    })
}

/// `(receiver, styleable index)` for TypedArray accessors — args[0] is
/// `this`, args[1] the int index.
fn ta_index(vm: &Vm, args: &[V]) -> Option<(u32, usize)> {
    let _ = vm;
    let h = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => return None,
    };
    Some((h, i64v(args.get(1).copied().unwrap_or(V::I32(0))) as usize))
}

// ---------------------------------------------------------------------
// ViewConfiguration / Gravity

fn viewconfig_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match (class, name) {
        // int getAbsoluteGravity(int gravity, int layoutDirection):
        // resolves START/END bits against the direction (AOSP semantics).
        ("Landroid/view/Gravity;", "getAbsoluteGravity") => {
            let result = i64v(args.first().copied().unwrap_or(V::I32(0))) as i32;
            let dir = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as i32;
            Some(Some(V::I32(absolute_gravity(result, dir))))
        }
        // apply(int gravity, int w, int h, Rect container, Rect out)
        // apply(int gravity, int w, int h, Rect container, Rect out, int layoutDirection)
        // apply(int gravity, int w, int h, Rect container, int xAdj, int yAdj, Rect out)
        // apply(int gravity, int w, int h, Rect container, int xAdj, int yAdj, Rect out, int layoutDirection)
        ("Landroid/view/Gravity;", "apply") => {
            let g = i64v(args.first().copied().unwrap_or(V::I32(0))) as i32;
            let w = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as i32;
            let h = i64v(args.get(2).copied().unwrap_or(V::I32(0))) as i32;
            let container = args.get(3).copied().unwrap_or(V::Ref(0));
            let (x_adj, y_adj, out, dir) = match args.len() {
                5 => (0, 0, args.get(4).copied().unwrap_or(V::Ref(0)), 0),
                // (g, w, h, container, out, layoutDirection) — the
                // form androidx GravityCompat delegates to.
                6 => (
                    0,
                    0,
                    args.get(4).copied().unwrap_or(V::Ref(0)),
                    i64v(args.get(5).copied().unwrap_or(V::I32(0))) as i32,
                ),
                7 => (
                    i64v(args.get(4).copied().unwrap_or(V::I32(0))) as i32,
                    i64v(args.get(5).copied().unwrap_or(V::I32(0))) as i32,
                    args.get(6).copied().unwrap_or(V::Ref(0)),
                    0,
                ),
                _ => (
                    i64v(args.get(4).copied().unwrap_or(V::I32(0))) as i32,
                    i64v(args.get(5).copied().unwrap_or(V::I32(0))) as i32,
                    args.get(6).copied().unwrap_or(V::Ref(0)),
                    i64v(args.get(7).copied().unwrap_or(V::I32(0))) as i32,
                ),
            };
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] gravity apply g={g:#x} {w}x{h}");
            }
            gravity_apply(vm, g, w, h, container, x_adj, y_adj, out, dir);
            Some(None)
        }
        // applyDisplay(int gravity, Rect display, Rect inoutObj)
        // applyDisplay(int gravity, Rect display, Rect inoutObj, int layoutDirection)
        ("Landroid/view/Gravity;", "applyDisplay") => {
            let g = i64v(args.first().copied().unwrap_or(V::I32(0))) as i32;
            let display = args.get(1).copied().unwrap_or(V::Ref(0));
            let inout = args.get(2).copied().unwrap_or(V::Ref(0));
            let (V::Ref(dh), V::Ref(ih)) = (display, inout) else {
                return Ok(Some(None));
            };
            // AOSP keeps an in-bounds rect untouched — it only shifts
            // the edge that overflows `display` (or clips it when the
            // DISPLAY_CLIP_* bits are set), never re-lays it out.
            const DISPLAY_CLIP_VERTICAL: i32 = 0x80;
            const DISPLAY_CLIP_HORIZONTAL: i32 = 0x100;
            let (dl, dt, dr, db) = rect_edges(vm, dh);
            let (mut l, mut t, mut r, mut b) = rect_edges(vm, ih);
            if g & DISPLAY_CLIP_VERTICAL != 0 {
                t = t.max(dt);
                b = b.min(db);
            } else if b - t > db - dt {
                t = dt;
                b = db;
            } else if t < dt {
                b += dt - t;
                t = dt;
            } else if b > db {
                t -= b - db;
                b = db;
            }
            if g & DISPLAY_CLIP_HORIZONTAL != 0 {
                l = l.max(dl);
                r = r.min(dr);
            } else if r - l > dr - dl {
                l = dl;
                r = dr;
            } else if l < dl {
                r += dl - l;
                l = dl;
            } else if r > dr {
                l -= r - dr;
                r = dr;
            }
            rect_write(vm, ih, l, t, r, b);
            Some(None)
        }
        ("Landroid/view/Gravity;", "isVertical" | "isHorizontal") => Some(Some(V::I32(0))),
        ("Landroid/view/Gravity;", _) => Some(None),
        ("Landroid/view/ViewConfiguration;", "get") => Some(Some(host(
            vm,
            "Landroid/view/ViewConfiguration;",
            HostState::Empty,
        ))),
        ("Landroid/view/ViewConfiguration;", _) => {
            let n: i32 = match name {
                "getScaledTouchSlop" | "getScaledPagingTouchSlop" => 16,
                "getScaledMinimumFlingVelocity" => 50,
                "getScaledMaximumFlingVelocity" => 8000,
                "getScaledEdgeSlop" | "getFadingEdgeLength" => 12,
                "getScaledOverscrollDistance" => 0,
                "getScaledOverflingDistance" => 6,
                "getScaledMinScalingSpan" => 27,
                "getLongPressTimeout" | "getKeyRepeatTimeout" => 500,
                "getTapTimeout" => 180,
                "getDoubleTapTimeout" => 300,
                "getKeyRepeatDelay" => 50,
                "getPressedStateDuration" => 125,
                "getScrollBarFadeDuration" | "getScrollDefaultDelay" => 300,
                _ => 0,
            };
            if name == "getScaledScrollFactor" {
                Some(Some(V::F32(0.0)))
            } else {
                Some(Some(V::I32(n)))
            }
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// View.MeasureSpec — a 32-bit int packing a 2-bit mode in the top two
// bits: UNSPECIFIED=0, EXACTLY=1<<30, AT_MOST=2<<30 (negative i32).
// androidx containers check the mode in onMeasure (e.g. PagerTabStrip
// throws "Must measure with an exact width" when the width mode isn't
// EXACTLY), so the generic 0-returning stub breaks real layouts.

fn measurespec_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    const MODE_MASK: i32 = 0x3 << 30;
    const EXACTLY: i32 = 1 << 30;
    // All methods are static; args[0] is the spec int (or the size for
    // makeMeasureSpec) — there is no receiver.
    let spec = i32v(args.first().copied().unwrap_or(V::I32(0)));
    Some(Some(match name {
        "getMode" => V::I32(spec & MODE_MASK),
        "getSize" => V::I32(spec & !MODE_MASK),
        "makeMeasureSpec" | "makeMeasureSpecStatic" => {
            let mode = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            V::I32((spec & !MODE_MASK) | (mode & MODE_MASK))
        }
        "toString" => {
            let label = match spec & MODE_MASK {
                0 => "UNSPECIFIED",
                m if m == EXACTLY => "EXACTLY",
                _ => "AT_MOST",
            };
            jstr(vm, &format!("MeasureSpec: {} {}", label, spec & !MODE_MASK))
        }
        _ => return None,
    }))
}

// ---------------------------------------------------------------------
// Log

fn log_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "v" | "d" | "i" | "w" | "e" | "wtf" => {
            let tag = sv(vm, args[0]).unwrap_or_default();
            let msg = sv(vm, args[1]).unwrap_or_default();
            eprintln!("[{}/{}] {}", name.to_uppercase(), tag, msg);
            // Log.e(tag, msg, Throwable) — the throwable is where the
            // real failure lives; render its toString so captures like
            // Compose's "Error was captured in composition" aren't opaque.
            if matches!(name, "w" | "e" | "wtf") {
                if let Some(V::Ref(th)) = args.get(2).copied() {
                    if th != 0 {
                        let mut s = vm
                            .call_virtual(th, "getMessage", &[])
                            .ok()
                            .flatten()
                            .and_then(|v| sv(vm, v))
                            .unwrap_or_default();
                        if s.is_empty() {
                            if let Some(Obj::Throwable { detail, .. }) = vm.heap.get(th) {
                                s = detail.clone();
                            }
                        }
                        let site = vm
                            .throw_sites
                            .get(&th)
                            .map(|t| format!(" at {t}"))
                            .unwrap_or_default();
                        eprintln!(
                            "[{}/{}]   {}: {s}{site}",
                            name.to_uppercase(),
                            tag,
                            class_of(vm, V::Ref(th))
                        );
                    }
                }
            }
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
                _ => sv(vm, args[0]).unwrap_or_default() == sv(vm, args[1]).unwrap_or_default(),
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
            let parts: Vec<String> = items.iter().map(|v| v_to_s(vm, *v)).collect();
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
            Some(Some(V::I32(t.find(&n).map(|i| i as i32).unwrap_or(-1))))
        }
        "ellipsize" | "commaEllipsize" | "stringOrSpannedString" => Some(args.first().copied()),
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
                &t.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;"),
            )))
        }
        "regionMatches" => {
            let eq = sv(vm, args[0]) == sv(vm, args[3]);
            Some(Some(V::I32(eq as i32)))
        }
        "replace" | "expandTemplate" | "formatSimple" => Some(args.first().copied()),
        "writeToParcel" => Some(None),
        "getReverse" | "getTrimmedLength" => Some(Some(V::I32(0))),
        "isGraphic" | "isPrintableAscii" | "isPrintableAsciiOnly" => Some(Some(V::I32(1))),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Uri — stored as StrBuf text

fn uri_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let text = s(
        vm,
        match args[0] {
            V::Ref(h) => h,
            _ => 0,
        },
    )
    .unwrap_or_default();
    Ok(match name {
        "parse" => Some(Some(host(
            vm,
            "Landroid/net/Uri;",
            HostState::StrBuf(sv(vm, args[0]).unwrap_or_default()),
        ))),
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
            let a = sv(vm, args[0])
                .or_else(|| {
                    s(
                        vm,
                        match args[0] {
                            V::Ref(h) => h,
                            _ => 0,
                        },
                    )
                })
                .unwrap_or_default();
            let b = sv(vm, args[1]).unwrap_or_default();
            Some(Some(host(
                vm,
                "Landroid/net/Uri;",
                HostState::StrBuf(format!("{a}/{b}")),
            )))
        }
        "toString" | "getPath" | "getEncodedPath" | "getLastPathSegment" => {
            let t = match name {
                "getLastPathSegment" => text.rsplit('/').next().unwrap_or("").to_string(),
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
            // Opaque uris ("scheme:ssp") have no "//" after the scheme; a
            // bare path has no scheme at all.
            let sc = match text.split_once(':') {
                Some((s, _)) if !s.is_empty() && !s.contains('/') => s.to_string(),
                _ => String::new(),
            };
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
        "getQuery"
        | "getEncodedQuery"
        | "getFragment"
        | "getEncodedFragment"
        | "getQueryParameter"
        | "getQueryParameterNames"
        | "getQueryParameters"
        | "getPathSegments"
        | "getUserInfo" => {
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
                "isAbsolute" => text.contains(':'),
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
        "scheme"
        | "authority"
        | "path"
        | "encodedPath"
        | "query"
        | "encodedQuery"
        | "fragment"
        | "encodedFragment"
        | "appendQueryParameter"
        | "appendPath"
        | "appendEncodedPath"
        | "appendBooleanQueryParameter"
        | "clearQuery"
        | "encodedAuthority"
        | "opaquePart"
        | "encodedOpaquePart" => {
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
            let t = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(t))))
        }
        "fromFile" => {
            // File arg -> "file://<absolute path>" Uri text; an empty
            // buffer makes every later getScheme()/getPath() lie.
            let p = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let p = if p.is_empty() {
                p
            } else {
                // Escape characters that would cut the path short when the
                // Uri is parsed back ('#'/'?' end the path component).
                let mut enc = String::with_capacity(p.len());
                for c in p.chars() {
                    match c {
                        '#' => enc.push_str("%23"),
                        '?' => enc.push_str("%3F"),
                        '%' => enc.push_str("%25"),
                        _ => enc.push(c),
                    }
                }
                format!("file://{enc}")
            };
            Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(p))))
        }
        "fromParts" => {
            // (scheme, ssp, fragment) -> scheme:ssp[#fragment]; the ssp keeps
            // authority+path verbatim in an opaque URI.
            let sc = sv(vm, args.first().copied().unwrap_or(V::I32(0))).unwrap_or_default();
            let ssp = sv(vm, args.get(1).copied().unwrap_or(V::I32(0))).unwrap_or_default();
            let frag = sv(vm, args.get(2).copied().unwrap_or(V::I32(0)));
            let mut u = format!("{sc}:{ssp}");
            if let Some(f) = frag.filter(|f| !f.is_empty()) {
                u.push('#');
                u.push_str(&f);
            }
            Some(Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(u))))
        }
        "fromBundle" => Some(Some(host(
            vm,
            "Landroid/net/Uri;",
            HostState::StrBuf("".into()),
        ))),
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
        "getData" | "getDataString" => Some(Some(pseudo_get(vm, h, P_DATA).unwrap_or(V::Ref(0)))),
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
        "getComponent" => Some(Some(pseudo_get(vm, h, P_COMPONENT).unwrap_or(V::Ref(0)))),
        "setPackage" | "addCategory" | "removeCategory" | "addFlags" | "setFlags"
        | "setSelector" | "setClipData" | "setSourceBounds" | "setIdentifier" => {
            Some(Some(args[0]))
        }
        "getPackage" | "getCategories" | "getFlags" | "getSelector" | "getClipData"
        | "getSourceBounds" | "getIdentifier" => Some(Some(match name {
            "getFlags" => V::I32(0),
            "getCategories" => {
                return Ok(Some(Some(host(
                    vm,
                    "Ljava/util/Set;",
                    HostState::List(vec![]),
                ))))
            }
            _ => V::Ref(0),
        })),
        "putExtra"
        | "putCharSequenceArrayListExtra"
        | "putStringArrayListExtra"
        | "putIntegerArrayListExtra"
        | "putParcelableArrayListExtra" => {
            map_put(vm, h, args[1], args[2]);
            Some(Some(args[0]))
        }
        "getStringExtra"
        | "getCharSequenceExtra"
        | "getSerializableExtra"
        | "getParcelableExtra"
        | "getBundleExtra" => Some(Some(map_get(vm, h, args[1]).unwrap_or(V::Ref(0)))),
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
            // AOSP returns the Bundle (or null), never the Intent —
            // `createSavedStateHandle` check-casts the result to
            // Bundle, so an Intent here throws ClassCastException.
            let m = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.clone(),
                _ => vec![],
            };
            if m.is_empty() {
                Some(Some(V::Ref(0)))
            } else {
                let b = vm.alloc_host("Landroid/os/Bundle;", HostState::Map(m));
                Some(Some(V::Ref(b)))
            }
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
        "getFloatExtra" => Some(Some(V::F32(f64v(
            map_get(vm, h, args[1])
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::F32(0.0)),
        ) as f32))),
        "getDoubleExtra" => Some(Some(V::F64(f64v(
            map_get(vm, h, args[1])
                .or_else(|| args.get(2).copied())
                .unwrap_or(V::F64(0.0)),
        )))),
        "makeMainActivity"
        | "makeRestartActivityForTask"
        | "getIntent"
        | "getIntentOld"
        | "parseIntent"
        | "getIntentInternal" => Some(Some(args[0])),
        "createChooser" => Some(Some(args[0])),
        "resolveActivity" | "resolveActivityInfo" => Some(Some(V::Ref(0))),
        "describeContents" | "writeToParcel" | "readFromParcel" => Some(None),
        _ => {
            let _ = sig;
            None
        }
    })
}

/// `android.app.Dialog` (and `AlertDialog`) — a real Dialog ctor mints
/// its own `Window` and installs itself as `Window.Callback`; without
/// that androidx dialogs throw "Window callback may not be null" and
/// `getWindow()` returns nothing usable.
fn dialog_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(Some(match name {
        "<init>" => {
            if let Some(V::Ref(this)) = args.first().copied() {
                view::dialog_window(vm, this);
                // Dialog(Context[, int themeResId]) — remember the ctx.
                let ctx = args.get(1).copied().unwrap_or(V::Ref(0));
                view::set_dialog_ctx(vm, this, ctx);
            }
            None
        }
        "getWindow" => {
            let w = args
                .first()
                .and_then(|a| match a {
                    V::Ref(h) => view::dialog_window(vm, *h),
                    _ => None,
                })
                .unwrap_or(0);
            Some(V::Ref(w))
        }
        "getContext" => Some(
            args.first()
                .map(|a| match a {
                    V::Ref(h) => view::dialog_ctx(vm, *h),
                    _ => V::Ref(0),
                })
                .unwrap_or(V::Ref(0)),
        ),
        "setContentView" => {
            if let Some(V::Ref(this)) = args.first().copied() {
                view::set_content_view(vm, this, args.get(1).copied().unwrap_or(V::Ref(0)));
            }
            None
        }
        "findViewById" => {
            let this = match args.first() {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            let id = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            let hit = view::content_root(vm, this)
                .and_then(|root| view::find_in_tree(vm, root, id))
                .unwrap_or(0);
            Some(V::Ref(hit))
        }
        "isShowing" | "isCancelable" => Some(V::I32(1)),
        "show"
        | "dismiss"
        | "cancel"
        | "hide"
        | "create"
        | "onCreate"
        | "onStart"
        | "onStop"
        | "onAttachedToWindow"
        | "onDetachedFromWindow"
        | "setTitle"
        | "setCancelable"
        | "setCanceledOnTouchOutside"
        | "setOnCancelListener"
        | "setOnDismissListener"
        | "setOnShowListener"
        | "setOnKeyListener"
        | "setOwnerActivity"
        | "setVolumeControlStream"
        | "setFeatureDrawableResource"
        | "setFeatureDrawable"
        | "requestWindowFeature"
        | "addContentView"
        | "dismissWithBehaviour"
        | "takeKeyEvents"
        | "onWindowAttributesChanged"
        | "onWindowFocusChanged"
        | "onRestoreInstanceState"
        | "setActionBar"
        | "setOnActionModeStarted"
        | "setOnActionModeFinished" => None,
        _ => return Ok(None),
    }))
}

fn copy_job_fields(vm: &mut Vm, from: u32, to: u32) {
    for k in [P_COMPONENT, P_DATA, P_FLAGS, P_META as u32] {
        if let Some(v) = pseudo_get(vm, from, k) {
            pseudo_set(vm, to, k, v);
        }
    }
}

fn job_service_desc(vm: &Vm, component: u32) -> Option<String> {
    let cls = pseudo_get(vm, component, P_DATA).and_then(|v| sv(vm, v))?;
    if cls.is_empty() {
        return None;
    }
    Some(if cls.starts_with('L') {
        cls
    } else {
        format!("L{};", cls.replace('.', "/"))
    })
}

fn job_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => 0,
    };
    Ok(match (class, name) {
        ("Landroid/app/job/JobInfo$Builder;", "<init>") => {
            pseudo_set(vm, this, P_FLAGS, args.get(1).copied().unwrap_or(V::I32(0)));
            pseudo_set(
                vm,
                this,
                P_COMPONENT,
                args.get(2).copied().unwrap_or(V::Ref(0)),
            );
            Some(None)
        }
        ("Landroid/app/job/JobInfo$Builder;", "setExtras") => {
            pseudo_set(vm, this, P_DATA, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(Some(args[0]))
        }
        ("Landroid/app/job/JobInfo$Builder;", "setMinimumLatency") => {
            pseudo_set(vm, this, P_META, args.get(1).copied().unwrap_or(V::I64(0)));
            Some(Some(args[0]))
        }
        ("Landroid/app/job/JobInfo$Builder;", "build") => {
            let jh = vm.alloc_host("Landroid/app/job/JobInfo;", HostState::Map(Vec::new()));
            copy_job_fields(vm, this, jh);
            Some(Some(V::Ref(jh)))
        }
        ("Landroid/app/job/JobInfo$Builder;", n)
            if n.starts_with("set") || n == "addTriggerContentUri" =>
        {
            // Constraint details decide *when* AOSP runs the job; the
            // runtime's scheduler is best-effort and currently only needs
            // extras/component/identity preserved. Every builder setter
            // is chainable.
            Some(Some(args[0]))
        }
        ("Landroid/app/job/JobInfo;", "getExtras") => {
            Some(Some(pseudo_get(vm, this, P_DATA).unwrap_or(V::Ref(0))))
        }
        ("Landroid/app/job/JobInfo;", "getId") => {
            Some(Some(pseudo_get(vm, this, P_FLAGS).unwrap_or(V::I32(0))))
        }
        ("Landroid/app/job/JobParameters;", "getExtras") => {
            Some(Some(pseudo_get(vm, this, P_DATA).unwrap_or(V::Ref(0))))
        }
        ("Landroid/app/job/JobParameters;", "getJobId") => {
            Some(Some(pseudo_get(vm, this, P_FLAGS).unwrap_or(V::I32(0))))
        }
        ("Landroid/app/job/JobParameters;", "getTriggeredContentUris")
        | ("Landroid/app/job/JobParameters;", "getTriggeredContentAuthorities")
        | ("Landroid/app/job/JobParameters;", "getNetwork") => Some(Some(V::Ref(0))),
        ("Landroid/app/job/JobScheduler;", "schedule") => {
            if let (V::Ref(jh), Some(V::Ref(ch))) = (
                args.get(1).copied().unwrap_or(V::Ref(0)),
                pseudo_get_optional_job_component(vm, args),
            ) {
                if jh != 0 && ch != 0 {
                    // JobScheduler.replace semantics: another schedule for
                    // the same job id supersedes the still-pending request.
                    let id = pseudo_get(vm, jh, P_FLAGS).unwrap_or(V::I32(0));
                    let keep: Vec<_> = vm
                        .pending_jobs
                        .iter()
                        .filter(|(pj, _, _)| {
                            pseudo_get(vm, *pj, P_FLAGS).unwrap_or(V::I32(0)) != id
                        })
                        .copied()
                        .collect();
                    vm.pending_jobs = keep;
                    let latency = pseudo_get(vm, jh, P_META).map(i64v).unwrap_or(0);
                    if crate::dbg_flag("OVELO_JOBDBG") {
                        eprintln!("[job] schedule id={id:?} latency={latency}");
                    }
                    vm.pending_jobs.push((jh, ch, now_ms() + latency.max(0)));
                }
            }
            Some(Some(V::I32(1)))
        }
        ("Landroid/app/job/JobScheduler;", "cancel")
        | ("Landroid/app/job/JobScheduler;", "cancelAll") => Some(None),
        ("Landroid/app/job/JobService;", "jobFinished") => Some(None),
        _ => None,
    })
}

fn pseudo_get_optional_job_component(vm: &Vm, args: &[V]) -> Option<V> {
    match args.get(1).copied() {
        Some(V::Ref(jh)) if jh != 0 => pseudo_get(vm, jh, P_COMPONENT),
        _ => None,
    }
}

/// Dispatch accepted JobScheduler jobs between guest activations. This
/// is deliberately best-effort rather than a constraint scheduler: an
/// Android runtime without system job infrastructure still must run
/// WorkManager expedited/immediate work, or repository syncs never start.
pub(crate) fn drain_jobs(vm: &mut Vm) {
    let now = now_ms();
    let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut vm.pending_jobs)
        .into_iter()
        .partition(|(_, _, t)| *t <= now);
    vm.pending_jobs = later;
    for (jh, ch, _) in due {
        let Some(desc) = job_service_desc(vm, ch) else {
            continue;
        };
        let sh = match vm.services.get(&desc) {
            Some(&h) => h,
            None => {
                let Ok(h) = vm.instantiate(&desc) else {
                    continue;
                };
                let _ = vm.call_virtual(h, "onCreate", &[]);
                vm.services.insert(desc.clone(), h);
                h
            }
        };
        let ph = vm.alloc_host(
            "Landroid/app/job/JobParameters;",
            HostState::Map(Vec::new()),
        );
        copy_job_fields(vm, jh, ph);
        let _ = vm.call_virtual(sh, "onStartJob", &[V::Ref(ph)]);
    }
}

fn component_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => 0,
    };
    // Stored separately by <init>/unflattenFromString so the
    // accessors answer real pieces: P_COMPONENT = package,
    // P_DATA = class.
    let part = |key: u32| -> Option<String> { pseudo_get(vm, this, key).and_then(|v| sv(vm, v)) };
    Ok(match name {
        "<init>" => {
            let rest = &args[1.min(args.len())..];
            // (Context, Class) | (Context, String) | (String, String)
            let (pkg, cls): (Option<String>, Option<String>) = match rest {
                [_, V::Ref(ch)] if matches!(vm.heap.get(*ch), Some(Obj::Class { .. })) => (
                    Some(vm.guest_package.clone().unwrap_or_default()),
                    class_desc_of(vm, *ch).map(|d| dotted(&d)),
                ),
                [V::Ref(p), V::Ref(c)] => {
                    (s(vm, *p).or_else(|| vm.guest_package.clone()), s(vm, *c))
                }
                [V::Ref(p), V::Ref(c), ..] => (s(vm, *p), s(vm, *c)),
                _ => (None, None),
            };
            if let Some(p) = pkg.filter(|p| !p.is_empty()) {
                let v = jstr(vm, &p);
                pseudo_set(vm, this, P_COMPONENT, v);
            }
            if let Some(c) = cls.filter(|c| !c.is_empty()) {
                let v = jstr(vm, &c);
                pseudo_set(vm, this, P_DATA, v);
            }
            Some(None)
        }
        "getPackageName" => Some(Some(jstr(vm, &part(P_COMPONENT).unwrap_or_default()))),
        "getClassName" => Some(Some(jstr(vm, &part(P_DATA).unwrap_or_default()))),
        "getShortClassName" => {
            let c = part(P_DATA).unwrap_or_default();
            let short = c.rsplit('.').next().unwrap_or(&c).to_string();
            Some(Some(jstr(vm, &short)))
        }
        "toString" | "flatten" | "flattenToShortString" | "flattenToString" => {
            let t = match (part(P_COMPONENT), part(P_DATA)) {
                (Some(p), Some(c)) => format!("{p}/{c}"),
                _ => s(vm, this).unwrap_or_default(),
            };
            Some(Some(jstr(vm, &t)))
        }
        "unflattenFromString" => {
            let cn = host(vm, "Landroid/content/ComponentName;", HostState::Empty);
            if let (V::Ref(ch), V::Ref(fh)) = (cn, args.first().copied().unwrap_or(V::Ref(0))) {
                if let Some(f) = s(vm, fh) {
                    if let Some((p, c)) = f.split_once('/') {
                        let pv = jstr(vm, p);
                        let cv = jstr(vm, c);
                        pseudo_set(vm, ch, P_COMPONENT, pv);
                        pseudo_set(vm, ch, P_DATA, cv);
                    }
                }
            }
            Some(Some(cn))
        }
        "equals" => {
            let other = match args.get(1) {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            let eq = this != 0
                && other != 0
                && pseudo_get(vm, this, P_COMPONENT) == pseudo_get(vm, other, P_COMPONENT)
                && pseudo_get(vm, this, P_DATA) == pseudo_get(vm, other, P_DATA);
            Some(Some(V::I32(i32::from(eq))))
        }
        "hashCode" | "compareTo" => {
            let flat = match (part(P_COMPONENT), part(P_DATA)) {
                (Some(p), Some(c)) => format!("{p}/{c}"),
                _ => s(vm, this).unwrap_or_default(),
            };
            let h = flat
                .bytes()
                .fold(0i32, |a, b| a.wrapping_mul(31).wrapping_add(b as i32));
            Some(Some(V::I32(h)))
        }
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

/// Resolve a color res-id to its ARGB int: chases `@color` reference
/// hops and `?attr` indirection through the bound theme, then through
/// `android.R.color` ids (`0x0106xxxx`) via the framework table.
/// Unresolvable attrs fall back to opaque black so text stays visible.
fn resolve_color_id(vm: &mut Vm, res_id: u32) -> u32 {
    let mut id = res_id;
    let mut data = 0xFF00_0000u32;
    for _ in 0..8 {
        let Some(res) = vm.apk_res.as_ref() else {
            break;
        };
        let Some(ev) = res.entry_value(id) else {
            if let Some(c) = crate::res::framework_color(id) {
                data = c;
            }
            break;
        };
        match ev.data_type {
            0x01 => {
                id = ev.data;
            }
            0x02 => match view::theme_attr_entry(vm, ev.data) {
                Some(next) if next.data_type == 0x01 => {
                    id = next.data;
                }
                Some(next) => {
                    data = next.data;
                    break;
                }
                None => break,
            },
            0x03 => {
                data = ev
                    .string
                    .as_deref()
                    .and_then(vector::parse_color)
                    .or_else(|| {
                        vector::drawable_flat_color(
                            vector::RCtx {
                                res,
                                theme: vm.theme_id,
                                depth: 0,
                            },
                            id,
                        )
                    })
                    .unwrap_or(0xFF00_0000);
                break;
            }
            _ => {
                data = ev.data;
                break;
            }
        }
    }
    data
}

fn active_network(vm: &mut Vm, connectivity: V) -> V {
    let h = match connectivity {
        V::Ref(h) => h,
        _ => return host(vm, "Landroid/net/Network;", HostState::Empty),
    };
    if let Some(network @ V::Ref(_)) = pseudo_get(vm, h, P_DATA) {
        return network;
    }
    let network = host(vm, "Landroid/net/Network;", HostState::Empty);
    pseudo_set(vm, h, P_DATA, network);
    network
}

fn network_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    match (class, name) {
        ("Landroid/net/ConnectivityManager;", "getActiveNetwork") => {
            Some(Some(active_network(vm, args[0])))
        }
        ("Landroid/net/ConnectivityManager;", "getNetworkCapabilities") => match args.get(1) {
            Some(V::Ref(0)) => Some(Some(V::Ref(0))),
            Some(_) => Some(Some(host(
                vm,
                "Landroid/net/NetworkCapabilities;",
                HostState::Empty,
            ))),
            None => Some(Some(V::Ref(0))),
        },
        ("Landroid/net/ConnectivityManager;", "getActiveNetworkInfo") => Some(Some(host(
            vm,
            "Landroid/net/NetworkInfo;",
            HostState::Empty,
        ))),
        ("Landroid/net/ConnectivityManager;", "getAllNetworks") => {
            let network = active_network(vm, args[0]);
            Some(Some(arr(vm, "Landroid/net/Network;", vec![network])))
        }
        ("Landroid/net/ConnectivityManager;", "getAllNetworkInfo") => {
            let info = host(vm, "Landroid/net/NetworkInfo;", HostState::Empty);
            Some(Some(arr(vm, "Landroid/net/NetworkInfo;", vec![info])))
        }
        ("Landroid/net/ConnectivityManager;", "isActiveNetworkMetered") => Some(Some(V::I32(0))),
        ("Landroid/net/ConnectivityManager;", "registerDefaultNetworkCallback")
        | ("Landroid/net/ConnectivityManager;", "registerNetworkCallback")
        | ("Landroid/net/ConnectivityManager;", "unregisterNetworkCallback") => Some(None),
        ("Landroid/net/NetworkCapabilities;", "hasCapability") => {
            let cap = i32v(args.get(1).copied().unwrap_or(V::I32(-1)));
            // INTERNET=12, NOT_METERED=11, VALIDATED=16. The host is a
            // desktop with a direct connection, so expose it as unmetered.
            Some(Some(V::I32(matches!(cap, 11 | 12 | 16) as i32)))
        }
        ("Landroid/net/NetworkCapabilities;", "hasTransport") => {
            // TRANSPORT_WIFI=1; the host connection is presented as Wi-Fi,
            // not cellular, so metered-mobile logic does not trigger.
            Some(Some(V::I32(
                (i32v(args.get(1).copied().unwrap_or(V::I32(-1))) == 1) as i32,
            )))
        }
        ("Landroid/net/NetworkInfo;", "isConnected")
        | ("Landroid/net/NetworkInfo;", "isAvailable")
        | ("Landroid/net/NetworkInfo;", "isConnectedOrConnecting") => Some(Some(V::I32(1))),
        ("Landroid/net/NetworkInfo;", "getType") => Some(Some(V::I32(1))),
        ("Landroid/net/NetworkInfo;", "getTypeName") => Some(Some(jstr(vm, "WIFI"))),
        ("Landroid/net/Network;", "toString") => Some(Some(jstr(vm, "host-network"))),
        _ => Some(Some(stub_ret(vm, ""))),
    }
}

fn prefs_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let class = class_of(vm, args[0]);
    Ok(if class.ends_with("$Editor;") {
        match name {
            "putString" | "putInt" | "putLong" | "putFloat" | "putBoolean" | "putStringSet" => {
                // Android editors stage writes; only commit/apply publishes
                // them to the shared backing map and disk.
                map_put(vm, h, args[1], args[2]);
                Some(Some(args[0]))
            }
            "remove" | "clear" => {
                if name == "clear" {
                    if let Some(m) = map_mut(vm, h) {
                        m.clear();
                    }
                    pseudo_set(vm, h, P_DATA, V::I32(1));
                } else {
                    let marker = host(
                        vm,
                        "Landroid/content/SharedPreferences$Editor$Remove;",
                        HostState::Empty,
                    );
                    map_put(vm, h, args[1], marker);
                }
                Some(Some(args[0]))
            }
            "commit" | "apply" => {
                let pending = match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Map(m),
                        ..
                    }) => m.clone(),
                    _ => vec![],
                };
                let clear = matches!(pseudo_get(vm, h, P_DATA), Some(V::I32(1)));
                let parent = pseudo_get(vm, h, P_EDIT_PARENT).unwrap_or(V::Ref(h));
                if let V::Ref(ph) = parent {
                    if clear {
                        if let Some(m) = map_mut(vm, ph) {
                            m.clear();
                        }
                    }
                    for (key, value) in pending {
                        if class_of(vm, value)
                            == "Landroid/content/SharedPreferences$Editor$Remove;"
                        {
                            map_remove(vm, ph, key);
                        } else {
                            map_put(vm, ph, key, value);
                        }
                    }
                    if let Some((file, _)) = vm
                        .prefs_files
                        .iter()
                        .find(|(_, phh)| **phh == ph)
                        .map(|(f, phh)| (f.clone(), *phh))
                    {
                        save_prefs(vm, &file, ph);
                    }
                }
                if let Some(m) = map_mut(vm, h) {
                    m.clear();
                }
                pseudo_set(vm, h, P_DATA, V::I32(0));
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
            "getString" => {
                let r = map_get(vm, h, args[1])
                    .or_else(|| args.get(2).copied())
                    .unwrap_or(V::Ref(0));
                if std::env::var("OVELO_PREFDBG").is_ok() {
                    eprintln!(
                        "prefdbg getString key={:?} -> {:?}",
                        sv(vm, args[1]),
                        sv(vm, r)
                    );
                }
                Some(Some(r))
            }
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
                    HostState::Map(vec![]),
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
        "getPackageName" | "getOpPackageName" | "getAttributionTag" | "getProcessName" => {
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
        "getApplicationContext" | "getApplication" => Some(Some(app_object(vm))),
        "getBaseContext" => Some(Some(args[0])),
        "getApplicationInfo" | "getActivityInfo" => Some(Some(app_info(vm))),
        "getContentResolver" => Some(Some(host(
            vm,
            "Landroid/content/ContentResolver;",
            HostState::Empty,
        ))),
        "getMainLooper" | "getLooper" => Some(Some(main_looper(vm))),
        "getMainExecutor" | "getMainHandler" => {
            Some(Some(host(vm, "Landroid/os/Handler;", HostState::Empty)))
        }
        "getResources" => Some(Some(host(
            vm,
            "Landroid/content/res/Resources;",
            HostState::Empty,
        ))),
        "obtainStyledAttributes" | "obtainAttributes" => Some(Some(view::typed_array(vm, args))),
        "getTheme" => Some(Some(host(
            vm,
            "Landroid/content/res/Resources$Theme;",
            HostState::Empty,
        ))),
        "setTheme" => {
            // Theme-swapping apps (AntennaPod's ThemeSwitcher) setTheme
            // before setContentView — theme-scoped attr resolution must
            // follow, else AppCompat's feature check sees the manifest
            // (splash) theme and throws.
            if let Some(v) = args.get(1).copied() {
                vm.theme_id = i32v(v) as u32;
            }
            Some(None)
        }
        "getAssets" => Some(Some(host(
            vm,
            "Landroid/content/res/AssetManager;",
            HostState::Empty,
        ))),
        "getSharedPreferences" => {
            let file = sv(vm, args[1]).unwrap_or_default();
            Some(Some(prefs_file(vm, &file)))
        }
        "getString" | "getText" => {
            // Context.getString resolves through resources, not to "" —
            // apps read defaults this way (NewPipe's saved_tabs JSON
            // default comes from a string resource).
            let id = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32;
            Some(Some(
                match vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.string_of(id).map(|s| s.to_string()))
                {
                    Some(s) => jstr(vm, &s),
                    None => jstr(vm, ""),
                },
            ))
        }
        "getColor" => Some(Some(V::I32(resolve_color_id(
            vm,
            i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32,
        ) as i32))),
        "getDrawable" => {
            let vdc = [
                "Landroidx/vectordrawable/graphics/drawable/VectorDrawableCompat;",
                "Landroid/support/graphics/drawable/VectorDrawableCompat;",
            ]
            .iter()
            .find_map(|c| vm.instantiate(c).ok().map(V::Ref));
            let r = vdc.unwrap_or_else(|| {
                host(
                    vm,
                    "Landroid/graphics/drawable/VectorDrawable;",
                    HostState::Empty,
                )
            });
            if crate::dbg_flag("OVELO_TRACE_DRAW") {
                eprintln!(
                    "[ctx.getDrawable] class={class} args={:?} -> {r:?}",
                    args.get(1)
                );
            }
            Some(Some(r))
        }
        "getColorStateList" => Some(Some(host(
            vm,
            "Landroid/content/res/ColorStateList;",
            HostState::Empty,
        ))),
        "getSystemService" => {
            // Hand out a typed placeholder per service name so calls on
            // it resolve (void stubs) and instanceof/cast work; real
            // service behavior is still surface-limited.
            let service = sv(vm, args[1]).or_else(|| match args[1] {
                V::Ref(ch) => class_desc_of(vm, ch),
                _ => None,
            });
            let desc = match service.as_deref() {
                Some("activity") => "Landroid/app/ActivityManager;",
                Some("alarm") => "Landroid/app/AlarmManager;",
                Some("appops") => "Landroid/app/AppOpsManager;",
                Some("audio") => "Landroid/media/AudioManager;",
                Some("clipboard") => "Landroid/content/ClipboardManager;",
                Some("connectivity") | Some("Landroid/net/ConnectivityManager;") => {
                    "Landroid/net/ConnectivityManager;"
                }
                Some("device_policy") => "Landroid/app/admin/DevicePolicyManager;",
                Some("display") => "Landroid/hardware/display/DisplayManager;",
                Some("download") => "Landroid/app/DownloadManager;",
                Some("input_method") => "Landroid/view/inputmethod/InputMethodManager;",
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
                Some("textservices") => "Landroid/view/textservice/TextServicesManager;",
                Some("wallpaper") => "Landroid/app/WallpaperManager;",
                _ => "Ljava/lang/Object;",
            };
            if desc == "Landroid/view/LayoutInflater;" {
                Some(Some(view::inflater(vm, args.first().copied())))
            } else {
                Some(Some(host(vm, desc, HostState::Empty)))
            }
        }
        "getSystemServiceName" => Some(Some(jstr(vm, "java.lang.Object"))),
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
        "getDatabasePath" => {
            let name = sv(vm, args.get(1).copied().unwrap_or(V::Ref(0)))
                .unwrap_or_else(|| "db".to_string());
            let logical = format!("/data/local/ovelo/databases/{name}");
            if let Some(parent) = io::fspath(vm, &logical).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Some(Some(host(vm, "Ljava/io/File;", HostState::Path(logical))))
        }
        "getExternalFilesDir"
        | "getExternalCacheDir"
        | "getObbDir"
        | "getCodeCacheDir"
        | "getDataDir"
        | "getNoBackupFilesDir"
        | "getExternalMediaDir" => {
            if name == "getExternalMediaDir" {
                Some(Some(V::Ref(0)))
            } else {
                let leaf = match name {
                    "getExternalFilesDir" => "external/files",
                    "getExternalCacheDir" => "external/cache",
                    "getObbDir" => "obb",
                    "getCodeCacheDir" => "code_cache",
                    "getDataDir" => "data",
                    "getNoBackupFilesDir" => "no_backup",
                    _ => name,
                };
                let logical = format!("/data/local/ovelo/{leaf}");
                let _ = std::fs::create_dir_all(io::fspath(vm, &logical));
                Some(Some(host(vm, "Ljava/io/File;", HostState::Path(logical))))
            }
        }
        "getExternalFilesDirs" | "getExternalCacheDirs" | "getExternalMediaDirs" | "getObbDirs" => {
            Some(Some(arr(vm, "Ljava/io/File;", vec![])))
        }
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
        "openOrCreateDatabase" => {
            let n = args
                .iter()
                .skip(1)
                .find_map(|a| sv(vm, *a))
                .unwrap_or_default();
            Some(Some(super::db::open_or_create(vm, &n)?))
        }
        "deleteFile" | "deleteDatabase" | "deleteSharedPreferences" => Some(Some(V::I32(0))),
        "moveDatabaseFrom" | "moveSharedPreferencesFrom" => Some(Some(V::I32(0))),
        "startActivity"
        | "startActivities"
        | "startActivityForResult"
        | "startActivityFromChild"
        | "startActivityFromFragment"
        | "startActivityIfNeeded"
        | "startNextMatchingActivity" => {
            // Component-targeted intents become a pending launch the
            // runner drains into a real second-Activity dispatch. The
            // Intent travels with the launch so the target's getIntent
            // sees the caller's extras/URI.
            if let Some(V::Ref(ih)) = args.get(1).copied() {
                if let Some(c) = pseudo_get(vm, ih, P_COMPONENT) {
                    let desc = match c {
                        // A ComponentName carries the target class
                        // dotted in P_DATA; Class mirrors and bare
                        // class-name strings resolve through their
                        // own descriptors.
                        V::Ref(ch) if ch != 0 => pseudo_get(vm, ch, P_DATA)
                            .and_then(|v| sv(vm, v))
                            .or_else(|| vm.class_descriptor_of(ch))
                            .or_else(|| sv(vm, V::Ref(ch)))
                            .map(|d| {
                                if d.starts_with('L') && d.ends_with(';') {
                                    d
                                } else {
                                    format!("L{};", d.replace('.', "/"))
                                }
                            }),
                        _ => None,
                    };
                    if let Some(d) = desc {
                        vm.pending_activity = Some((d, V::Ref(ih)));
                    }
                }
            }
            Some(Some(V::I32(0)))
        }
        "startService" | "startForegroundService" => {
            // Launch the named Service for real — apps like NewPipe do
            // their feed fetch in `FeedLoadService.onStartCommand`, so a
            // stub here means the whole feature never runs. The Intent's
            // P_COMPONENT holds a ComponentName (class name in P_DATA),
            // a Class mirror or a bare class-name string.
            let mut component = V::Ref(0);
            let svc = (|| -> Option<u32> {
                let V::Ref(ih) = args.get(1).copied()? else {
                    return None;
                };
                let V::Ref(ch) = pseudo_get(vm, ih, P_COMPONENT)? else {
                    return None;
                };
                component = V::Ref(ch);
                let desc = vm
                    .class_descriptor_of(ch)
                    .or_else(|| sv(vm, V::Ref(ch)))
                    .or_else(|| pseudo_get(vm, ch, P_DATA).and_then(|v| sv(vm, v)))?;
                let desc = if desc.starts_with('L') {
                    desc
                } else {
                    format!("L{};", desc.replace('.', "/"))
                };
                let sh = match vm.services.get(&desc) {
                    Some(&h) => h,
                    None => {
                        let h = vm.instantiate(&desc).ok()?;
                        let _ = vm.call_virtual(h, "onCreate", &[]);
                        if vm.service_stop_pending.remove(&h) {
                            // onCreate already self-stopped: AOSP runs
                            // onDestroy without onStartCommand and
                            // never registers the service.
                            return None;
                        }
                        vm.services.insert(desc.clone(), h);
                        h
                    }
                };
                vm.service_seq += 1;
                let sid = vm.service_seq;
                vm.service_start_id.insert(sh, sid);
                let _ = vm.call_virtual(
                    sh,
                    "onStartCommand",
                    &[V::Ref(ih), V::I32(0), V::I32(sid as i32)],
                );
                Some(sh)
            })();
            Some(Some(if svc.is_some() { component } else { V::Ref(0) }))
        }
        "stopSelf" | "stopSelfResult" => {
            // AOSP: stopSelf marks the service stopped — without
            // dropping the cached instance the next startService would
            // reuse a service that already finished its work (NewPipe's
            // FeedLoadService calls stopSelf when the fetch is done;
            // the next refresh then got no onStartCommand work cycle).
            let this = i64v(args[0]).max(0) as u32;
            // stopSelf(startId)/stopSelfResult(startId) only stop when
            // the id matches the latest delivered start request — an
            // earlier request finishing must not tear down a service
            // a newer onStartCommand still needs.
            if args.len() > 1 && vm.service_start_id.get(&this).copied() != Some(i64v(args[1])) {
                return Ok(Some(Some(V::I32(0))));
            }
            if vm.services.values().any(|sh| *sh == this) {
                vm.services.retain(|_, sh| *sh != this);
                vm.service_start_id.remove(&this);
            } else {
                // Self-stop before startService registered the handle
                // (inside onCreate): the starter consults this set and
                // skips insert/onStartCommand entirely.
                vm.service_stop_pending.insert(this);
            }
            // Defer onDestroy to the drain loop: running it inline
            // tears the service down while its own onStartCommand (or
            // the startService call) is still on the stack.
            if !vm.pending_destroy.contains(&this) {
                vm.pending_destroy.push(this);
            }
            Some(Some(V::I32(1)))
        }
        "stopService" => {
            let desc = args
                .get(1)
                .and_then(|a| {
                    let V::Ref(ih) = *a else { return None };
                    let V::Ref(ch) = pseudo_get(vm, ih, P_COMPONENT)? else {
                        return None;
                    };
                    vm.class_descriptor_of(ch)
                        .or_else(|| sv(vm, V::Ref(ch)))
                        .or_else(|| pseudo_get(vm, ch, P_DATA).and_then(|v| sv(vm, v)))
                })
                .map(|d| {
                    if d.starts_with('L') {
                        d
                    } else {
                        format!("L{};", d.replace('.', "/"))
                    }
                });
            if let Some(desc) = desc {
                if let Some(sh) = vm.services.remove(&desc) {
                    vm.service_start_id.remove(&sh);
                    // Same deferred-teardown rule as stopSelf: the
                    // caller may be this service's own callback.
                    if !vm.pending_destroy.contains(&sh) {
                        vm.pending_destroy.push(sh);
                    }
                }
            }
            Some(Some(V::I32(1)))
        }
        "bindService"
        | "unbindService"
        | "sendBroadcast"
        | "sendOrderedBroadcast"
        | "sendStickyBroadcast"
        | "registerReceiver"
        | "unregisterReceiver"
        | "startInstrumentation"
        | "startIntentSender"
        | "startIntentSenderFromChild"
        | "revokeUriPermission"
        | "grantUriPermission"
        | "enforcePermission"
        | "enforceCallingPermission"
        | "enforceCallingOrSelfPermission"
        | "enforceUriPermission"
        | "checkPermission"
        | "checkSelfPermission"
        | "checkCallingPermission"
        | "checkCallingOrSelfPermission"
        | "checkUriPermission"
        | "checkCallingOrSelfUriPermission"
        | "checkCallingUriPermission"
        | "registerComponentCallbacks"
        | "unregisterComponentCallbacks"
        | "getDisplay"
        | "getDisplayId"
        | "isRestricted"
        | "getAttributionSource"
        | "isDeviceProtectedStorage"
        | "createDeviceProtectedStorageContext"
        | "createPackageContext"
        | "createContextForSplit"
        | "createAttributionContext"
        | "createDisplayContext"
        | "createWindowContext"
        | "moveResources"
        | "reloadResources"
        | "bindServiceAsUser"
        | "bindIsolatedService"
        | "updateServiceGroup" => {
            if name.starts_with("check") {
                // checkSelfPermission/checkPermission/…: PERMISSION_
                // GRANTED only when the name is in the granted set
                // (manifest normal-level + requestPermissions). Calls
                // that carry no permission-looking arg (checkSignatures
                // and friends) keep the permissive default.
                let g = perm_arg(vm, args)
                    .map(|p| vm.granted_permissions.contains(&p))
                    .unwrap_or(true);
                Some(Some(V::I32(if g { 0 } else { -1 })))
            } else if matches!(
                name,
                "bindService" | "bindServiceAsUser" | "bindIsolatedService" | "stopService"
            ) {
                Some(Some(V::I32(1)))
            } else if matches!(
                name,
                "isRestricted"
                    | "isDeviceProtectedStorage"
                    | "createDeviceProtectedStorageContext"
                    | "moveResources"
                    | "reloadResources"
            ) {
                Some(Some(V::I32(0)))
            } else if name.starts_with("create")
                || name == "getDisplay"
                || name == "registerReceiver"
            {
                Some(Some(V::Ref(0)))
            } else {
                Some(None)
            }
        }
        "getClassLoader" => Some(Some(host(vm, "Ljava/lang/ClassLoader;", HostState::Empty))),
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
        "requestPermissions" => {
            // (String[] perms, int requestCode): model "user grants
            // everything asked" — dangerous permissions join the
            // granted set — then deliver onRequestPermissionsResult
            // the way AOSP does after the dialog resolves.
            let perms_v = args.get(1).copied().unwrap_or(V::Ref(0));
            let mut names: Vec<String> = Vec::new();
            if let V::Ref(ah) = perms_v {
                if let Some(Obj::Array { data, .. }) = vm.heap.get(ah) {
                    names = data.iter().filter_map(|v| sv(vm, *v)).collect();
                }
            }
            for p in &names {
                vm.granted_permissions.insert(p.clone());
            }
            let code = match args.get(2) {
                Some(V::I32(c)) => *c,
                _ => 0,
            };
            let results = vm.heap.alloc(Obj::Array {
                elem: "I".to_string(),
                data: vec![V::I32(0); names.len()],
            });
            if let V::Ref(this) = args.first().copied().unwrap_or(V::Ref(0)) {
                if this != 0 {
                    let _ = vm.call_virtual(
                        this,
                        "onRequestPermissionsResult",
                        &[V::I32(code), perms_v, V::Ref(results)],
                    );
                }
            }
            Some(None)
        }
        "registerActivityLifecycleCallbacks" => {
            // AOSP keeps these on the Application; our runner dispatches
            // to every registered callback around each lifecycle stage.
            if let Some(V::Ref(cb)) = args.get(1).copied() {
                vm.activity_callbacks.push(cb);
            }
            Some(None)
        }
        "unregisterActivityLifecycleCallbacks" => {
            if let Some(V::Ref(cb)) = args.get(1).copied() {
                vm.activity_callbacks.retain(|h| *h != cb);
            }
            Some(None)
        }
        "setIntent"
        | "requestWindowFeature"
        | "setTitle"
        | "setTitleColor"
        | "setVisible"
        | "setResult"
        | "setRequestedOrientation"
        | "setTaskDescription"
        | "setVolumeControlStream"
        | "invalidateOptionsMenu"
        | "supportRequestWindowFeature"
        | "onWindowFocusChanged"
        | "openOptionsMenu"
        | "closeOptionsMenu"
        | "openContextMenu"
        | "closeContextMenu"
        | "registerForContextMenu"
        | "unregisterForContextMenu"
        | "showDialog"
        | "dismissDialog"
        | "removeDialog"
        | "managedQuery"
        | "requestDragAndDropPermissions"
        | "startLocalVoiceInteraction"
        | "stopLocalVoiceInteraction"
        | "startSearch"
        | "triggerSearch"
        | "takeKeyEvents"
        | "overridePendingTransition"
        | "requestShowKeyboardShortcuts"
        | "requestVisibleBehind"
        | "setContentTransitionManager"
        | "setEnterSharedElementCallback"
        | "setExitSharedElementCallback"
        | "setFinishOnTouchOutside"
        | "setImmersive"
        | "setInheritShowWhenLocked"
        | "setLocusContext"
        | "setPictureInPictureParams"
        | "setRecentsScreenshotEnabled"
        | "setShouldDockBigOverlays"
        | "setShowWhenLocked"
        | "setTranslucent"
        | "setTurnScreenOn"
        | "setVrModeEnabled"
        | "showAssist"
        | "showLockTaskEscapeMessage"
        | "startLockTask"
        | "stopLockTask"
        | "requestFullscreenMode"
        | "reportFullyDrawn"
        | "dismissKeyboardShortcutsHelper"
        | "dismissKeyguard"
        | "onProvideKeyboardShortcuts" => Some(None),
        // Activity.findViewById searches the decor tree under the content
        // view set by setContentView; miss → null like real Android.
        "findViewById" => {
            let act = match args.first() {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            let id = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            let hit = view::content_root(vm, act)
                .and_then(|root| view::find_in_tree(vm, root, id))
                .unwrap_or(0);
            Some(Some(V::Ref(hit)))
        }
        "setContentView" | "addContentView" => {
            let act = match args.first() {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            view::set_content_view(vm, act, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "setContentView_Int" => Some(None),
        "getWindow" => {
            let act = match args.first() {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            Some(Some(V::Ref(view::window_of(vm, act).unwrap_or(0))))
        }
        "getLayoutInflater" | "getMenuInflater" => {
            Some(Some(view::inflater(vm, args.first().copied())))
        }
        "getWindowManager"
        | "getActionBar"
        | "getFragmentManager"
        | "getLoaderManager"
        | "getMediaController"
        | "getSearchEvent"
        | "getCallingActivity"
        | "getCallingPackage"
        | "getReferrer"
        | "getTaskId"
        | "getMaxNumPictureInPictureActions"
        | "getParentActivityIntent"
        | "getLocalClassName"
        | "getComponentName"
        | "getVoiceInteractor"
        | "getActivityToken"
        | "getAssistToken"
        | "getCurrentFocus"
        | "getLastNonConfigurationInstance"
        | "onRetainNonConfigurationInstance"
        | "isTaskRoot"
        | "isFinishing"
        | "isDestroyed"
        | "isChangingConfigurations"
        | "isInMultiWindowMode"
        | "isInPictureInPictureMode"
        | "isChild"
        | "isImmersive"
        | "isLaunchedFromBubble"
        | "isLocalVoiceInteractionSupported"
        | "hasWindowFocus"
        | "moveTaskToBack"
        | "finish"
        | "finishAffinity"
        | "finishActivity"
        | "finishActivityFromChild"
        | "finishAfterTransition"
        | "finishAndRemoveTask"
        | "finishFromChild"
        | "onBackPressed"
        | "onUserInteraction"
        | "onPostResume"
        | "navigateUpTo"
        | "navigateUpToFromChild"
        | "shouldUpRecreateTask" => {
            match name {
                "getTaskId" => Some(Some(V::I32(1))),
                "isFinishing" => Some(Some(V::I32(match args[0] {
                    V::Ref(h) => view::is_finishing(vm, h) as i32,
                    _ => 0,
                }))),
                "isTaskRoot"
                | "hasWindowFocus"
                | "isInMultiWindowMode"
                | "isInPictureInPictureMode"
                | "isImmersive"
                | "isChild"
                | "isLaunchedFromBubble"
                | "isLocalVoiceInteractionSupported"
                | "isDestroyed"
                | "isChangingConfigurations" => Some(Some(V::I32(matches!(
                    name,
                    "isTaskRoot" | "hasWindowFocus"
                ) as i32))),
                "finish"
                | "finishAffinity"
                | "finishAfterTransition"
                | "finishAndRemoveTask"
                | "finishFromChild" => {
                    if let V::Ref(h) = args[0] {
                        pseudo_set(vm, h, view::P_FINISHED, V::I32(1));
                    }
                    Some(None)
                }
                // finishActivity*(requestCode) ends a *child* activity
                // started for a result, not the caller — flagging it
                // would mark `this` finished and abort its lifecycle.
                "finishActivity"
                | "finishActivityFromChild"
                | "onBackPressed"
                | "onUserInteraction"
                | "onPostResume"
                | "onRetainNonConfigurationInstance"
                | "moveTaskToBack" => Some(None),
                "navigateUpTo"
                | "navigateUpToFromChild"
                | "shouldUpRecreateTask"
                | "startActivityIfNeeded"
                | "startNextMatchingActivity" => Some(Some(V::I32(0))),
                "getLocalClassName" => {
                    let c = class_of(vm, args[0]);
                    Some(Some(jstr(vm, &dotted(&c))))
                }
                "getComponentName" => Some(Some(host(
                    vm,
                    "Landroid/content/ComponentName;",
                    HostState::StrBuf(dotted(&class_of(vm, args[0]))),
                ))),
                "getIntent" => {
                    // The launch Intent recorded by the runner when the
                    // activity was started via startActivity; otherwise a
                    // fresh empty one.
                    let i = match args[0] {
                        V::Ref(h) => view::activity_intent(vm, h),
                        _ => None,
                    };
                    Some(Some(i.unwrap_or_else(|| {
                        host(vm, "Landroid/content/Intent;", HostState::Map(vec![]))
                    })))
                }
                "getMaxNumPictureInPictureActions" => Some(Some(V::I32(0))),
                "onProvideKeyboardShortcuts" => Some(None),
                _ => Some(Some(stub_ret(vm, sig))),
            }
        }
        "runOnUiThread" => {
            if let V::Ref(r) = args[1] {
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!(
                        "[viewdbg] runOnUiThread runnable={r} dex_run={}",
                        vm.has_dex_virtual(r, "run", &[])
                    );
                }
                if let Err(e) = vm.call_virtual(r, "run", &[]) {
                    // The runnable is the app's UI-commit point — a throw
                    // here means the data never reaches the view. Swallowing
                    // it silently leaves every screen looking "empty for no
                    // reason", so surface it under the debug flag.
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        eprintln!("[viewdbg] runOnUiThread runnable={r} err {e:?}");
                    }
                }
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
        "onCreate"
        | "onStart"
        | "onResume"
        | "onPause"
        | "onStop"
        | "onDestroy"
        | "onRestart"
        | "onSaveInstanceState"
        | "onRestoreInstanceState"
        | "onActivityResult"
        | "onNewIntent"
        | "onRequestPermissionsResult"
        | "onCreateOptionsMenu"
        | "onOptionsItemSelected"
        | "onCreateContextMenu"
        | "onContextItemSelected"
        | "onOptionsMenuClosed"
        | "onPrepareOptionsMenu"
        | "onConfigurationChanged"
        | "onLowMemory"
        | "onTrimMemory"
        | "onMultiWindowModeChanged"
        | "onPictureInPictureModeChanged"
        | "onWindowAttributesChanged"
        | "onMenuOpened"
        | "onContextMenuClosed"
        | "onAttachedToWindow"
        | "onDetachedFromWindow"
        | "onEnterAnimationComplete"
        | "onGetDirectActions"
        | "onPerformDirectAction"
        | "onProvideAssistContent"
        | "onProvideAssistData"
        | "onStateNotSaved"
        | "onTopResumedActivityChanged" => {
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

/// The permission-name argument of a check*/enforce* call: the first
/// string arg containing "permission" — covers both
/// `checkSelfPermission(String)` (args[1]) and
/// `PackageManager.checkPermission(String, String)` (args[1]),
/// whatever wrapper the app bundles.
fn perm_arg(vm: &Vm, args: &[V]) -> Option<String> {
    args.iter()
        .skip(1)
        .filter_map(|a| sv(vm, *a))
        .find(|s| s.contains("permission"))
}

/// `PackageItemInfo` family (ResolveInfo, *Info, AppWidgetProviderInfo)
/// — `loadLabel`/`loadIcon` over the pseudo fields `mint_info` fills.
fn item_info_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    Ok(match name {
        "loadLabel" => {
            // The real application label from the installed APK's
            // manifest (`@android:label` → arsc string or literal) —
            // not the component's class name. The package travels as a
            // dotted string in P_COMPONENT, same walk `loadIcon` does.
            let pkg = {
                let mut found = None;
                let mut cur = Some(h);
                for _ in 0..4 {
                    let Some(ch) = cur else { break };
                    if let Some(V::Ref(s)) = pseudo_get(vm, ch, P_COMPONENT) {
                        if let Some(p) = vm.heap.str_val(s) {
                            if p.contains('.') {
                                found = Some(p.to_string());
                                break;
                            }
                        }
                    }
                    cur =
                        [P_APPINFO, P_META, P_TARGET as u64]
                            .iter()
                            .find_map(|k| match pseudo_get(vm, ch, *k) {
                                Some(V::Ref(x)) if x != 0 => Some(x),
                                _ => None,
                            });
                }
                found
            };
            if let Some(label) = pkg
                .and_then(|p| vm.installed_apk_paths.get(&p).cloned())
                .and_then(|b| std::fs::read(b).ok())
                .and_then(|b| crate::package_label(&b))
            {
                return Ok(Some(Some(jstr(vm, &label))));
            }
            // App label: the *Info's name (P_DATA), else its package —
            // chasing ResolveInfo→*Info and *Info→applicationInfo when
            // the receiver itself only carries links.
            let live = |v: Option<V>| v.filter(|v| matches!(v, V::Ref(x) if *x != 0));
            let via = |vh: u32, key: u32| live(pseudo_get(vm, vh, key));
            let v = via(h, P_DATA).or_else(|| via(h, P_COMPONENT)).or_else(|| {
                live(pseudo_get(vm, h, P_META)).and_then(|m| {
                    if let V::Ref(mh) = m {
                        live(pseudo_get(vm, mh, P_COMPONENT))
                    } else {
                        None
                    }
                })
            });
            let v = v
                .or_else(|| match live(pseudo_get(vm, h, P_TARGET)) {
                    Some(V::Ref(th)) => {
                        via(th, P_DATA)
                            .or_else(|| via(th, P_COMPONENT))
                            .or_else(|| {
                                live(pseudo_get(vm, th, P_META)).and_then(|m| {
                                    if let V::Ref(mh) = m {
                                        live(pseudo_get(vm, mh, P_COMPONENT))
                                    } else {
                                        None
                                    }
                                })
                            })
                    }
                    _ => None,
                })
                .unwrap_or_else(|| jstr(vm, "app"));
            Some(Some(v))
        }
        "loadIcon" | "loadUnbadgedIcon" => {
            // Real icon: decode the installed APK's application icon.
            // The package travels as P_COMPONENT (a package-like
            // dotted string) on the *Info itself or one hop through
            // P_META/P_TARGET — same linkage `loadLabel` walks.
            let pkg = {
                let mut found = None;
                let mut cur = Some(h);
                for _ in 0..4 {
                    let Some(ch) = cur else { break };
                    if let Some(V::Ref(s)) = pseudo_get(vm, ch, P_COMPONENT) {
                        if let Some(p) = vm.heap.str_val(s) {
                            if p.contains('.') {
                                found = Some(p.to_string());
                                break;
                            }
                        }
                    }
                    cur =
                        [P_APPINFO, P_META, P_TARGET as u64]
                            .iter()
                            .find_map(|k| match pseudo_get(vm, ch, *k) {
                                Some(V::Ref(x)) if x != 0 => Some(x),
                                _ => None,
                            });
                }
                found
            };
            match pkg.and_then(|p| super::vector::package_icon_drawable(vm, &p)) {
                Some(d) => Some(Some(d)),
                None => Some(Some(host(
                    vm,
                    "Landroid/graphics/drawable/Drawable;",
                    HostState::Empty,
                ))),
            }
        }
        _ => None,
    })
}

fn pm_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getPackageInfo" => {
            // Unknown package names must throw NameNotFoundException —
            // callers like fossify's isPackageInstalled probe sibling
            // apps this way and branch on the catch.
            let want = args.iter().skip(1).find_map(|a| sv(vm, *a));
            let mine = vm.guest_package.clone().unwrap_or_default();
            if let Some(w) = want.as_deref() {
                if w != mine && !w.starts_with("android") && !w.starts_with("com.android") {
                    return Err(vm.throw_builtin(
                        "Landroid/content/pm/PackageManager$NameNotFoundException;",
                        w,
                    ));
                }
            }
            let h = vm.alloc_host("Landroid/content/pm/PackageInfo;", HostState::Empty);
            let pkg = vm.guest_package.clone().unwrap_or_else(|| "app".into());
            let pv = jstr(vm, &pkg);
            pseudo_set(vm, h, P_ACTION, pv);
            if let Some(fidx) = vm
                .field_index_of("Landroid/content/pm/PackageInfo;", "packageName")
                .map(|g| vm.canon_field(g))
            {
                pseudo_set(vm, h, fidx, pv);
            }
            if let Some(fidx) = vm
                .field_index_of("Landroid/content/pm/PackageInfo;", "versionCode")
                .map(|g| vm.canon_field(g))
            {
                pseudo_set(vm, h, fidx, V::I32(1));
            }
            Some(Some(V::Ref(h)))
        }
        "getApplicationInfo" => Some(Some(app_info(vm))),
        "getActivityInfo"
        | "getServiceInfo"
        | "getReceiverInfo"
        | "getProviderInfo"
        | "resolveActivity"
        | "resolveService"
        | "resolveContentProvider"
        | "getInstallerPackageName"
        | "getLaunchIntentForPackage"
        | "getLeanbackLaunchIntentForPackage"
        | "getDrawable"
        | "getActivityIcon"
        | "getApplicationIcon"
        | "getDefaultActivityIcon"
        | "getActivityBanner"
        | "getApplicationBanner"
        | "getApplicationLabel"
        | "getPermissionGroupInfo"
        | "getPermissionInfo"
        | "getPackageArchiveInfo"
        | "getResourcesForActivity"
        | "getResourcesForApplication"
        | "getText"
        | "getXml"
        | "getSystemSharedLibraryNames"
        | "getChangedPackages"
        | "getSharedLibraries"
        | "getInstantAppCookie"
        | "getSuspendedPackageAppExtras"
        | "getUnbadgedIcon"
        | "getUserBadgedIcon"
        | "getUserBadgedLabel"
        | "getUserBadgedDrawableForDensity"
        | "getPackagesForUid"
        | "getNameForUid"
        | "getSyntheticAppDetailsActivityEnabled"
        | "queryIntentActivities"
        | "queryIntentActivityOptions"
        | "queryIntentServices"
        | "queryIntentContentProviders"
        | "queryBroadcastReceivers"
        | "getInstalledPackages"
        | "getInstalledApplications"
        | "getPackagesHoldingPermissions"
        | "checkPermission"
        | "checkSignatures"
        | "hasSystemFeature"
        | "isInstantApp"
        | "isPackageSuspended"
        | "canRequestPackageInstalls"
        | "hasSigningCertificate"
        | "addPermission"
        | "addPermissionAsync"
        | "removePermission"
        | "clearPackagePreferredActivities"
        | "addPackageToPreferred"
        | "removePackageFromPreferred"
        | "getPreferredPackages"
        | "getPreferredActivities"
        | "setComponentEnabledSetting"
        | "getComponentEnabledSetting"
        | "setApplicationEnabledSetting"
        | "getApplicationEnabledSetting"
        | "isSafeMode"
        | "getPackageInstaller"
        | "extendVerificationTimeout"
        | "verifyPendingInstall"
        | "setInstallerPackageName"
        | "updateInstantAppCookie"
        | "clearInstantAppCookie"
        | "getModuleInfo"
        | "getInstallReason"
        | "getInstallSourceInfo" => match name {
            "getActivityInfo" => Some(Some(host(
                vm,
                "Landroid/content/pm/ActivityInfo;",
                HostState::Empty,
            ))),
            "getServiceInfo" => Some(Some(host(
                vm,
                "Landroid/content/pm/ServiceInfo;",
                HostState::Empty,
            ))),
            "getReceiverInfo" => Some(Some(host(
                vm,
                "Landroid/content/pm/ActivityInfo;",
                HostState::Empty,
            ))),
            "getProviderInfo" => Some(Some(host(
                vm,
                "Landroid/content/pm/ProviderInfo;",
                HostState::Empty,
            ))),
            "getLaunchIntentForPackage" | "getLeanbackLaunchIntentForPackage" => {
                // Launchers resolve installed packages to an Intent
                // carrying a ComponentName; unknown package → null.
                let want = args
                    .iter()
                    .skip(1)
                    .find_map(|a| sv(vm, *a))
                    .unwrap_or_default();
                match vm.installed_launchers.iter().find(|(p, _)| *p == want) {
                    Some((pkg, act)) => {
                        let act = act.clone();
                        let pkg = pkg.clone();
                        let cn = host(vm, "Landroid/content/ComponentName;", HostState::Empty);
                        if let V::Ref(ch) = cn {
                            let av = jstr(vm, &act);
                            let pv = jstr(vm, &pkg);
                            pseudo_set(vm, ch, P_DATA, av);
                            pseudo_set(vm, ch, P_COMPONENT, pv);
                        }
                        let ih = host(vm, "Landroid/content/Intent;", HostState::Map(vec![]));
                        if let V::Ref(ih2) = ih {
                            pseudo_set(vm, ih2, P_COMPONENT, cn);
                        }
                        Some(Some(ih))
                    }
                    None => Some(Some(V::Ref(0))),
                }
            }
            "getApplicationIcon" => {
                let pkg = args.iter().skip(1).find_map(|a| sv(vm, *a));
                match pkg.and_then(|p| super::vector::package_icon_drawable(vm, &p)) {
                    Some(d) => Some(Some(d)),
                    None => Some(Some(V::Ref(0))),
                }
            }
            "getInstallerPackageName"
            | "getDrawable"
            | "getActivityIcon"
            | "getDefaultActivityIcon"
            | "getActivityBanner"
            | "getApplicationBanner"
            | "getPermissionGroupInfo"
            | "getPermissionInfo"
            | "getPackageArchiveInfo"
            | "getResourcesForActivity"
            | "getResourcesForApplication"
            | "getText"
            | "getXml"
            | "getSystemSharedLibraryNames"
            | "getChangedPackages"
            | "getSharedLibraries"
            | "getInstantAppCookie"
            | "getSuspendedPackageAppExtras"
            | "getUnbadgedIcon"
            | "getUserBadgedIcon"
            | "getUserBadgedLabel"
            | "getUserBadgedDrawableForDensity"
            | "getPackagesForUid"
            | "getNameForUid"
            | "resolveActivity"
            | "resolveService"
            | "resolveContentProvider"
            | "getModuleInfo"
            | "getInstallSourceInfo"
            | "getPackageInstaller" => {
                if name == "getPackageInstaller" {
                    // A real handle — install managers take
                    // `installer.getClass()`/`createSession` on it and
                    // an NPE here kills Hilt singleton construction.
                    Some(Some(host(
                        vm,
                        "Landroid/content/pm/PackageInstaller;",
                        HostState::Empty,
                    )))
                } else if name == "getApplicationLabel" {
                    Some(Some(jstr(vm, "app")))
                } else {
                    Some(Some(V::Ref(0)))
                }
            }
            "getApplicationLabel" => Some(Some(jstr(vm, "app"))),
            "queryIntentActivities"
            | "queryIntentActivityOptions"
            | "queryIntentServices"
            | "queryIntentContentProviders"
            | "queryBroadcastReceivers"
            | "getInstalledPackages"
            | "getInstalledApplications"
            | "getPackagesHoldingPermissions"
            | "getPreferredPackages" => {
                Some(Some(host(vm, "Ljava/util/List;", HostState::List(vec![]))))
            }
            "checkPermission" | "checkSignatures" => {
                let g = perm_arg(vm, args)
                    .map(|p| vm.granted_permissions.contains(&p))
                    .unwrap_or(true);
                Some(Some(V::I32(if g { 0 } else { -1 })))
            }
            "hasSystemFeature"
            | "isInstantApp"
            | "isPackageSuspended"
            | "canRequestPackageInstalls"
            | "hasSigningCertificate"
            | "isSafeMode"
            | "getSyntheticAppDetailsActivityEnabled" => Some(Some(V::I32(0))),
            "getComponentEnabledSetting" | "getApplicationEnabledSetting" => Some(Some(V::I32(1))),
            "getPreferredActivities" | "getInstallReason" => Some(Some(V::I32(0))),
            _ => Some(None),
        },
        _ => None,
    })
}

fn resources_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let res_id = || -> u32 { i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32 };
    Ok(match name {
        "getString" | "getText" => Some(Some(
            match vm
                .apk_res
                .as_ref()
                .and_then(|r| r.string_of(res_id()).map(|s| s.to_string()))
            {
                Some(s) => jstr(vm, &s),
                None => jstr(vm, ""),
            },
        )),
        "getQuantityString" | "getQuantityText" => {
            let id = res_id();
            let qty = i64v(args.get(2).copied().unwrap_or(V::I32(0)));
            let text = vm
                .apk_res
                .as_ref()
                .and_then(|r| r.plural_string(id, qty))
                .map(|v| {
                    v.string
                        .clone()
                        .or_else(|| {
                            if v.data_type == 0x03 || v.data_type == 0x01 {
                                // Inline string or a reference to
                                // another resource (`@string/…`).
                                vm.apk_res
                                    .as_ref()
                                    .and_then(|r| r.string_of(v.data).map(|s| s.to_string()))
                            } else {
                                Some(v.data.to_string())
                            }
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            // Format only the 3-arg `getQuantityString(id, n, args...)`:
            // `getQuantityText` and the 2-arg form hand the caller a
            // template to format later — `%d`/`%s` must survive.
            if name == "getQuantityText" || args.len() < 4 {
                return Ok(Some(Some(jstr(vm, &text))));
            }
            let mut fmt_args: Vec<String> = Vec::new();
            if let Some(V::Ref(a)) = args.get(3) {
                let items = match vm.heap().get(*a) {
                    Some(Obj::Array { data, .. }) => data.clone(),
                    _ => Vec::new(),
                };
                for it in items {
                    fmt_args.push(v_to_s(vm, it));
                }
            }
            let mut out = String::with_capacity(text.len() + 8);
            let mut next = 0usize;
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                // `%N$` pins the arg index without advancing the
                // sequential cursor; `%%` consumes no arg.
                let mut conv = chars.next().unwrap_or('%');
                let arg = if conv.is_ascii_digit() {
                    let idx = conv.to_digit(10).unwrap_or(0) as usize;
                    let _ = chars.next(); // '$'
                    conv = chars.next().unwrap_or('%');
                    fmt_args.get(idx.saturating_sub(1)).cloned()
                } else {
                    fmt_args.get(next).cloned()
                };
                match conv {
                    'd' | 'i' | 's' => {
                        out.push_str(&arg.unwrap_or_default());
                        next += 1;
                    }
                    '%' => out.push('%'),
                    _ => {
                        out.push('%');
                        out.push(conv);
                        next += 1;
                    }
                }
            }
            Some(Some(jstr(vm, &out)))
        }
        "getStringArray" | "getTextArray" => {
            let items = vm
                .apk_res
                .as_ref()
                .map(|r| r.array_items(res_id()))
                .unwrap_or_default();
            let vals: Vec<V> = items
                .iter()
                .map(|it| match it.data_type {
                    0x03 => it
                        .string
                        .as_deref()
                        .map(|s| jstr(vm, s))
                        .unwrap_or(V::Ref(0)),
                    0x01 => {
                        let s = vm
                            .apk_res
                            .as_ref()
                            .and_then(|r| r.string_of(it.data))
                            .map(|s| s.to_string());
                        s.map(|s| jstr(vm, &s)).unwrap_or(V::Ref(0))
                    }
                    _ => V::Ref(0),
                })
                .collect();
            Some(Some(arr(vm, STRING, vals)))
        }
        "getIntArray" => {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                let rid = res_id();
                let n = vm
                    .apk_res
                    .as_ref()
                    .map(|r| r.array_items(rid).len())
                    .unwrap_or(0);
                let name = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.resource_name(rid))
                    .unwrap_or_default();
                eprintln!("[viewdbg] getIntArray rid={rid:#x} name={name:?} items={n}");
            }
            let vals: Vec<V> = vm
                .apk_res
                .as_ref()
                .map(|r| {
                    let items = r.array_items(res_id());
                    if items.is_empty() {
                        // A plain value resource loads as a 1-element TypedArray.
                        return r
                            .entry_value(res_id())
                            .map(|e| vec![V::I32(e.data as i32)])
                            .unwrap_or_default();
                    }
                    items
                        .iter()
                        .map(|it| match it.data_type {
                            0x01 => r
                                .entry_value(it.data)
                                .map(|e| V::I32(e.data as i32))
                                .unwrap_or(V::I32(0)),
                            _ => V::I32(it.data as i32),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Some(arr(vm, "I", vals)))
        }
        "getBoolean" | "getInteger" => Some(Some(V::I32(
            vm.apk_res
                .as_ref()
                .and_then(|r| r.entry_value(res_id()))
                .map(|v| v.data as i32)
                .unwrap_or(0),
        ))),
        "getColor" => Some(Some(V::I32(resolve_color_id(vm, res_id()) as i32))),
        "getDimension" | "getFraction" => Some(Some(V::F32(
            vm.apk_res
                .as_ref()
                .and_then(|r| r.dimen(res_id()))
                .unwrap_or(0.0),
        ))),
        "getDimensionPixelSize" | "getDimensionPixelOffset" => Some(Some(V::I32(
            vm.apk_res
                .as_ref()
                .and_then(|r| r.dimen(res_id()))
                .map(|f| f as i32)
                .unwrap_or(0),
        ))),
        "getIdentifier" => {
            // (name, defType, defPackage): look the entry up by name —
            // needed for getIdentifier("drawer_layout", "id", pkg).
            let name_s = args.get(1).and_then(|a| sv(vm, *a)).unwrap_or_default();
            let hit = vm
                .apk_res
                .as_ref()
                .and_then(|r| r.id_named(&name_s))
                .unwrap_or(0);
            Some(Some(V::I32(hit as i32)))
        }
        "getDrawable" | "getDrawableForDensity" => {
            // Rasterize the resource when it is an image (vector AXML or
            // PNG/JPEG): a BitmapDrawable host whose bitmap the renderer's
            // field walk finds. Otherwise AppCompat's ResourceManagerInternal
            // throws when the drawable is not a bundled VectorDrawableCompat
            // AND its class is literally
            // `android.graphics.drawable.VectorDrawable` ("configure your
            // build for VectorDrawableCompat") — hand back a guest
            // VectorDrawableCompat so the instanceof check passes, or a
            // plain Drawable host for anything else.
            let res_id = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32;
            let d = if res_id > 0
                && super::vector::rctx(vm)
                    .and_then(|r| super::vector::drawable_bitmap(r, res_id))
                    .is_some()
            {
                super::vector::drawable_for_res(vm, res_id)
            } else {
                [
                    "Landroidx/vectordrawable/graphics/drawable/VectorDrawableCompat;",
                    "Landroid/support/graphics/drawable/VectorDrawableCompat;",
                ]
                .iter()
                .find_map(|c| vm.instantiate(c).ok().map(V::Ref))
                .unwrap_or_else(|| {
                    host(
                        vm,
                        "Landroid/graphics/drawable/VectorDrawable;",
                        HostState::Empty,
                    )
                })
            };
            Some(Some(d))
        }
        "getColorStateList" => Some(Some(host(
            vm,
            "Landroid/content/res/ColorStateList;",
            HostState::Empty,
        ))),
        "getFont" => Some(Some(host(
            vm,
            "Landroid/graphics/Typeface;",
            HostState::Empty,
        ))),
        "getLayout"
        | "obtainTypedArray"
        | "obtainAttributes"
        | "getXml"
        | "getRawResource"
        | "openRawResource"
        | "openRawResourceFd"
        | "getValue"
        | "getValueForDensity"
        | "getResourceName"
        | "getResourcePackageName"
        | "getResourceTypeName"
        | "getResourceEntryName"
        | "getDisplayMetrics"
        | "getConfiguration"
        | "getAssets"
        | "getSystem"
        | "newTheme"
        | "getMovie" => match name {
            "getLayout" => Some(Some(xml::parser_or_throw(
                vm,
                i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32,
            )?)),
            "openRawResource" | "getRawResource" => {
                let id = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32;
                let bytes = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.file_path(id).map(str::to_string))
                    .and_then(|p| vm.apk_res.as_ref().and_then(|r| r.read(&p)));
                Some(Some(match bytes {
                    Some(b) => {
                        let h = vm.alloc_host(
                            "Ljava/io/ByteArrayInputStream;",
                            HostState::Bytes(std::rc::Rc::new(b), 0),
                        );
                        V::Ref(h)
                    }
                    None => V::Ref(0),
                }))
            }
            "getDisplayMetrics" => Some(Some(host(
                vm,
                "Landroid/util/DisplayMetrics;",
                HostState::Empty,
            ))),
            "getConfiguration" => {
                let h = host(vm, "Landroid/content/res/Configuration;", HostState::Empty);
                if let V::Ref(hh) = h {
                    // `uiMode` night bit: apps gate text/icon colors on
                    // `isSystemInDarkMode` — derive it from the theme's
                    // resolved surface color so fossify's dark themes
                    // get light text instead of black-on-dark.
                    let lum = crate::theme_color_named(vm, "colorSurface")
                        .or_else(|| crate::theme_color_named(vm, "colorBackground"))
                        .map(|argb| {
                            let (r, g, b) = ((argb >> 16) as u8, (argb >> 8) as u8, argb as u8);
                            (299 * r as u32 + 587 * g as u32 + 114 * b as u32) as f32 / 255_000.0
                        });
                    let night = match lum {
                        Some(l) => l < 0.5,
                        None => true, // no resolvable theme: default dark
                    };
                    // UI_MODE_TYPE_NORMAL | NIGHT_YES/NIGHT_NO
                    let ui_mode = 1 | if night { 0x20 } else { 0x10 };
                    if let Some(fidx) = vm
                        .field_index_of("Landroid/content/res/Configuration;", "uiMode")
                        .map(|g| vm.canon_field(g))
                    {
                        pseudo_set(vm, hh, fidx, V::I32(ui_mode));
                    }
                    pseudo_set(vm, hh, crate::P_META, V::I32(ui_mode));
                }
                Some(Some(h))
            }
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
            "getResourceName" => Some(Some(
                match vm.apk_res.as_ref().and_then(|r| {
                    r.resource_name(i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32)
                }) {
                    Some(n) => jstr(vm, &n),
                    None => jstr(vm, ""),
                },
            )),
            "getResourceEntryName" => {
                let n = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| {
                        r.entry_name(i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32)
                    })
                    .map(str::to_string);
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!("[viewdbg] getResourceEntryName({:?}) -> {n:?}", args.get(1));
                }
                Some(Some(jstr(vm, n.as_deref().unwrap_or(""))))
            }
            "getResourceTypeName" => {
                let n = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| {
                        r.type_name(i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32)
                    })
                    .map(str::to_string);
                Some(Some(jstr(vm, n.as_deref().unwrap_or(""))))
            }
            "getResourcePackageName" => {
                let n = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.package_name())
                    .map(str::to_string);
                Some(Some(jstr(vm, n.as_deref().unwrap_or(""))))
            }
            "getXml" => Some(Some(xml::parser_or_throw(
                vm,
                i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32,
            )?)),
            "getValue" | "getValueForDensity" => {
                // Fill the caller's TypedValue from the arsc entry —
                // callers (ColorStateListInflaterCompat, …) branch on
                // `tv.type`, so an unfilled value misroutes them.
                let id = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32;
                // getValueForDensity(id, density, outValue, resolveRefs)
                // has an extra density arg before the out TypedValue.
                let tv_idx = if name == "getValueForDensity" { 3 } else { 2 };
                if let Some(tv) = args.get(tv_idx).copied() {
                    if let Some(ev) = vm.apk_res.as_ref().and_then(|r| r.entry_value(id)) {
                        let ev = ev.clone();
                        fill_typed_value_ev(vm, tv, &ev, id);
                    }
                }
                Some(None)
            }
            _ => Some(Some(V::Ref(0))),
        },
        "updateConfiguration"
        | "flushLayoutCache"
        | "finishPreloading"
        | "clearCachesForUntrustedConfigs"
        | "registerResourcePaths"
        | "getCompatibilityInfo"
        | "getCompatibilityInfo0" => Some(None),
        "getQuantityText0" => Some(Some(V::Ref(0))),
        _ => None,
    })
}

/// Fill a TypedValue host's guest-visible fields (`type`, `string`,
/// `data`, `resourceId`) from a resolved TypedArray value — the write
/// goes through the dex field index so `iget` reads it back.
fn fill_typed_value(vm: &mut Vm, tv: V, v: V) {
    let V::Ref(h) = tv else {
        return;
    };
    // A @string/ reference resolves eagerly — preference defaults and
    // titles read `.string`; other refs keep type/resourceId/data.
    let (ty, string, data, res_id) = match v {
        V::I32(id) if (id as u32) >> 24 == 0x7f || (id as u32) >> 24 == 0x01 => {
            match vm
                .apk_res
                .as_ref()
                .and_then(|r| r.string_of(id as u32).map(|s| s.to_string()))
            {
                Some(s) => (0x03, jstr(vm, &s), id, id as u32),
                None => (0x01, V::Ref(0), id, id as u32),
            }
        }
        V::I32(i) => (0x10, V::Ref(0), i, 0),
        V::I64(i) => (0x10, V::Ref(0), i as i32, 0),
        V::F32(f) => (0x04, V::Ref(0), f.to_bits() as i32, 0),
        V::F64(f) => (0x04, V::Ref(0), (f as f32).to_bits() as i32, 0),
        s @ V::Ref(_) => (0x03, s, 0, 0),
    };
    for (name, val) in [
        ("type", V::I32(ty)),
        ("string", string),
        ("data", V::I32(data)),
        ("resourceId", V::I32(res_id as i32)),
    ] {
        if let Some(f) = vm
            .field_index_of("Landroid/util/TypedValue;", name)
            .map(|g| vm.canon_field(g))
        {
            if let Some(m) = fields_mut(vm, h) {
                m.insert(f, val);
            }
        }
    }
}

/// Like `fill_typed_value` but from a raw arsc entry — the arsc
/// `data_type` numbering IS `android.util.TypedValue.TYPE_*`, so
/// boolean/dimension/color attrs keep their exact type and a
/// `TYPE_REFERENCE` keeps its resource id.
fn fill_typed_value_ev(vm: &mut Vm, tv: V, ev: &ovelo_apk::arsc::EntryValue, res_id: u32) {
    let V::Ref(h) = tv else {
        return;
    };
    let string = if ev.data_type == 0x03 {
        ev.string
            .as_deref()
            .map(|s| jstr(vm, s))
            .unwrap_or(V::Ref(0))
    } else {
        V::Ref(0)
    };
    let res_id = if ev.data_type == 0x01 {
        ev.data as i32
    } else {
        res_id as i32
    };
    for (name, val) in [
        ("type", V::I32(ev.data_type as i32)),
        ("string", string),
        ("data", V::I32(ev.data as i32)),
        ("resourceId", V::I32(res_id)),
    ] {
        if let Some(f) = vm
            .field_index_of("Landroid/util/TypedValue;", name)
            .map(|g| vm.canon_field(g))
        {
            if let Some(m) = fields_mut(vm, h) {
                m.insert(f, val);
            }
        }
    }
}

fn res_misc(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match (class, name) {
        ("Landroid/util/DisplayMetrics;", "scaledDensity") => Some(Some(V::F32(2.0))),
        ("Landroid/util/DisplayMetrics;", _) => None,
        ("Landroid/content/res/TypedArray;", "getString" | "getText") => {
            let found = ta_index(vm, args).and_then(|(h, i)| view::ta_lookup(vm, h, i));
            // A string attr may hold a @string/ reference — resolve it.
            let found = match found {
                Some(V::I32(id)) if (id as u32) >> 24 == 0x7f => vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.string_of(id as u32).map(|s| s.to_string()))
                    .map(|s| jstr(vm, &s)),
                other => other,
            };
            Some(Some(
                found
                    .filter(|v| matches!(v, V::Ref(h) if *h != 0))
                    .unwrap_or_else(|| args.get(2).copied().unwrap_or(V::Ref(0))),
            ))
        }
        (
            "Landroid/content/res/TypedArray;",
            "getInt"
            | "getColor"
            | "getBoolean"
            | "getInteger"
            | "getResourceId"
            | "getDimensionPixelSize"
            | "getDimensionPixelOffset"
            | "getLayoutDimension",
        ) => {
            let def = args.get(2).copied().unwrap_or(V::I32(0));
            let found = ta_index(vm, args)
                .and_then(|(h, i)| view::ta_lookup(vm, h, i))
                .map(|v| match v {
                    // A @res/ reference: `getResourceId` wants the id
                    // itself; the numeric getters dereference the arsc
                    // entry value (int data or scaled px for dims).
                    // arsc type ids are 1-based, so a literal whose
                    // type byte is 0 (e.g. color 0x7f00_0000) is a
                    // value, not a reference.
                    V::I32(id) if (id as u32) >> 24 == 0x7f && (id as u32) >> 16 & 0xff != 0 => {
                        // `?attr/x` entries arrive as the attr's res id;
                        // the theme bind rewrites them to the real
                        // resource/value (AOSP resolveAttributeReference).
                        let (rid, entry) = match view::resolve_ta_id(vm, id as u32) {
                            Some((r, ev)) => (r, Some(ev)),
                            None => (
                                id as u32,
                                vm.apk_res
                                    .as_ref()
                                    .and_then(|r| r.entry_value(id as u32).cloned()),
                            ),
                        };
                        if name == "getResourceId" {
                            V::I32(rid as i32)
                        } else {
                            match entry {
                                Some(v)
                                    if name.starts_with("getDimension")
                                        || name == "getLayoutDimension" =>
                                {
                                    V::I32(crate::res::typed_px(&v) as i32)
                                }
                                Some(v) => V::I32(v.data as i32),
                                None => V::I32(id),
                            }
                        }
                    }
                    V::I32(_) | V::I64(_) => V::I32(i64v(v) as i32),
                    V::F32(f) => V::I32(f as i32),
                    other => other,
                })
                .filter(|v| matches!(v, V::I32(_)));
            Some(Some(found.unwrap_or(def)))
        }
        ("Landroid/content/res/TypedArray;", "getFloat" | "getDimension") => {
            let def = args.get(2).copied().unwrap_or(V::F32(0.0));
            let found = ta_index(vm, args)
                .and_then(|(h, i)| view::ta_lookup(vm, h, i))
                .map(|v| match v {
                    // A @dimen/ reference resolves to its decoded px
                    // value (type byte nonzero — see getInt's guard).
                    V::I32(id) if (id as u32) >> 24 == 0x7f && (id as u32) >> 16 & 0xff != 0 => {
                        V::F32(
                            vm.apk_res
                                .as_ref()
                                .and_then(|r| r.dimen(id as u32))
                                .unwrap_or(id as f32),
                        )
                    }
                    other => V::F32(f64v(other) as f32),
                });
            Some(Some(found.unwrap_or(def)))
        }
        ("Landroid/content/res/TypedArray;", "getTextArray" | "getIntArray") => {
            // An @array/ reference decodes through the arsc item map.
            let items = ta_index(vm, args)
                .and_then(|(h, i)| view::ta_lookup(vm, h, i))
                .and_then(|v| match v {
                    V::I32(id) if (id as u32) >> 24 == 0x7f => Some(id as u32),
                    _ => None,
                })
                .and_then(|id| vm.apk_res.as_ref().map(|r| r.array_items(id)))
                .unwrap_or_default();
            if items.is_empty() {
                return Ok(Some(Some(V::Ref(0))));
            }
            let vals: Vec<V> = if name == "getTextArray" {
                items
                    .iter()
                    .map(|v| match v.string.as_deref() {
                        Some(s) => jstr(vm, s),
                        // A string-array may hold @string/ references —
                        // chase them so callers see the resolved text,
                        // not a decimal res id.
                        None if v.data_type == 0x01 => {
                            let resolved = vm
                                .apk_res
                                .as_ref()
                                .and_then(|r| r.string_of(v.data))
                                .map(|s| s.to_string());
                            jstr(vm, &resolved.unwrap_or_else(|| v.data.to_string()))
                        }
                        None => jstr(vm, &v.data.to_string()),
                    })
                    .collect()
            } else {
                items.iter().map(|v| V::I32(v.data as i32)).collect()
            };
            Some(Some(arr(vm, OBJ, vals)))
        }
        ("Landroid/content/res/TypedArray;", "getDrawable") => {
            // Drawable-typed styleable attrs resolve to a res id; render
            // vectors/bitmaps into a BitmapDrawable callers can draw.
            // `TintTypedArray` delegates to the wrapped TypedArray and
            // lands here too.
            let d = match ta_index(vm, args).and_then(|(h, i)| view::ta_lookup(vm, h, i)) {
                Some(V::I32(res)) if res > 0 => super::vector::drawable_for_res(vm, res as u32),
                _ => V::Ref(0),
            };
            Some(Some(d))
        }
        ("Landroid/content/res/TypedArray;", "getFont" | "getColorStateList") => {
            Some(Some(V::Ref(0)))
        }
        ("Landroid/content/res/TypedArray;", "hasValue" | "hasValueOrEmpty") => {
            let yes = ta_index(vm, args).is_some_and(|(h, i)| {
                if view::ta_lookup(vm, h, i).is_some() {
                    return true;
                }
                // Theme-scoped arrays (no AttributeSet) answer true for
                // every in-range index — material's ThemeEnforcement
                // (`hasValue(colorPrimary)`) needs that to accept the
                // (stubbed) theme.
                let set = pseudo_get(vm, h, view::P_TA_SET);
                !matches!(set, Some(V::Ref(s)) if s != 0)
            });
            Some(Some(V::I32(yes as i32)))
        }
        ("Landroid/content/res/TypedArray;", "length") => {
            // AOSP: `length()` is the requested styleable array's size —
            // callers iterate 0..length and check `hasValue` per index.
            let n = match args.first() {
                Some(V::Ref(h)) => view::ta_attrs(vm, *h).len() as i32,
                _ => 0,
            };
            Some(Some(V::I32(n)))
        }
        ("Landroid/content/res/TypedArray;", "getIndexCount") => {
            // AOSP: the count of attributes that resolved to a value.
            let n = match args.first() {
                Some(V::Ref(h)) => {
                    let attrs = view::ta_attrs(vm, *h);
                    // Theme-scoped arrays (no AttributeSet) answer `set`
                    // for every in-range index — mirror `hasValue`.
                    if matches!(pseudo_get(vm, *h, view::P_TA_SET), Some(V::Ref(s)) if s != 0) {
                        (0..attrs.len())
                            .filter(|i| view::ta_lookup(vm, *h, *i).is_some())
                            .count() as i32
                    } else {
                        attrs.len() as i32
                    }
                }
                _ => 0,
            };
            Some(Some(V::I32(n)))
        }
        ("Landroid/content/res/TypedArray;", "getIndex") => {
            let v = ta_index(vm, args)
                .and_then(|(h, i)| {
                    let attrs = view::ta_attrs(vm, h);
                    if matches!(pseudo_get(vm, h, view::P_TA_SET), Some(V::Ref(s)) if s != 0) {
                        // The i-th *set* attribute's position in the
                        // styleable array (AOSP `mIndices` entry).
                        (0..attrs.len())
                            .filter(|j| view::ta_lookup(vm, h, *j).is_some())
                            .nth(i)
                            .map(|j| j as i32)
                    } else {
                        attrs.get(i).map(|_| i as i32)
                    }
                })
                .unwrap_or(0);
            Some(Some(V::I32(v)))
        }
        ("Landroid/content/res/TypedArray;", "getType") => Some(Some(V::I32(0))),
        ("Landroid/content/res/TypedArray;", "getValue" | "peekValue") => {
            // getValue(index, TypedValue) fills the caller's TypedValue
            // in place and answers bool; peekValue answers a fresh one.
            let found = ta_index(vm, args).and_then(|(h, i)| view::ta_lookup(vm, h, i));
            let Some(v) = found else {
                return Ok(Some(Some(V::I32(0))));
            };
            if name == "peekValue" {
                let t = host(vm, "Landroid/util/TypedValue;", HostState::Empty);
                fill_typed_value(vm, t, v);
                return Ok(Some(Some(t)));
            }
            if let Some(tv) = args.get(2).copied() {
                fill_typed_value(vm, tv, v);
            }
            Some(Some(V::I32(1)))
        }
        ("Landroid/content/res/TypedArray;", "recycle") => Some(None),
        ("Landroid/content/res/TypedArray;", "extractThemeAttrs") => Some(Some(V::Ref(0))),
        ("Landroid/content/res/TypedArray;", "getChangingConfigurations") => Some(Some(V::I32(0))),
        (
            "Landroid/util/TypedValue;",
            "applyDimension"
            | "complexToDimension"
            | "complexToDimensionPixelSize"
            | "complexToDimensionPixelOffset"
            | "complexToFloat"
            | "complexToFraction",
        ) => Some(Some(V::F32(
            f64v(args.get(1).copied().unwrap_or(V::F32(0.0))) as f32,
        ))),
        ("Landroid/content/res/ColorStateList;", "valueOf") => {
            Some(Some(host(vm, class, HostState::Slot(args[0]))))
        }
        ("Landroid/content/res/ColorStateList;", "getDefaultColor") => Some(Some(match args[0] {
            V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
            v => v,
        })),
        ("Landroid/content/res/ColorStateList;", "getColorForState") => Some(Some(match args[0] {
            V::Ref(h) => slot_of(vm, h).unwrap_or(V::I32(0)),
            _ => V::I32(0),
        })),
        (
            "Landroid/content/res/AssetManager;",
            "open" | "openFd" | "openNonAssetFd" | "openXmlResourceParser" | "list",
        ) => {
            if name == "list" {
                return Ok(Some(Some(arr(vm, STRING, vec![]))));
            }
            // Asset reads serve real bytes out of the APK's assets/ dir.
            let fname = sv(vm, args.get(1).copied().unwrap_or(V::I32(0))).unwrap_or_default();
            let bytes = vm.apk_res.as_ref().and_then(|r| {
                r.read(&format!("assets/{fname}"))
                    .or_else(|| r.read(&fname))
            });
            if crate::dbg_flag("OVELO_ASSETDBG") {
                match &bytes {
                    Some(b) => {
                        eprintln!("[asset] {name} '{fname}' -> {} bytes h-alloc-next", b.len())
                    }
                    None => eprintln!("[asset] {name} '{fname}' -> MISSING (FileNotFound)"),
                }
            }
            let Some(b) = bytes else {
                // AOSP throws FileNotFoundException for a missing
                // asset — callers probe locale variants this way and
                // catch IOException to fall back (e.g. values-en →
                // values). A null stream NPEs in the wrong frame.
                return Err(vm.throw_builtin("Ljava/io/FileNotFoundException;", &fname));
            };
            Some(Some({
                let h = vm.alloc_host(
                    "Ljava/io/ByteArrayInputStream;",
                    HostState::Bytes(std::rc::Rc::new(b), 0),
                );
                if crate::dbg_flag("OVELO_ASSETDBG") {
                    eprintln!("[asset] {name} '{fname}' -> stream #{h}");
                }
                V::Ref(h)
            }))
        }
        (
            "Landroid/content/res/AssetManager;",
            "close" | "addAssetPath" | "isUpToDate" | "getLocales" | "getConfigurations",
        ) => match name {
            "addAssetPath" => Some(Some(V::I32(0))),
            "isUpToDate" => Some(Some(V::I32(1))),
            "getLocales" => Some(Some(host(vm, "Landroid/os/LocaleList;", HostState::Empty))),
            "getConfigurations" => {
                Some(Some(arr(vm, "Landroid/content/res/Configuration;", vec![])))
            }
            _ => Some(None),
        },
        ("Landroid/content/res/Resources$Theme;", "obtainStyledAttributes") => {
            Some(Some(view::typed_array(vm, args)))
        }
        ("Landroid/content/res/Resources$Theme;", "resolveAttribute") => {
            // resolveAttribute(attrResId, TypedValue out, resolveRefs):
            // fill the TypedValue from the theme and answer bool.
            let attr = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as u32;
            let resolve_refs = i32v(args.get(3).copied().unwrap_or(V::I32(0))) != 0;
            match view::theme_attr_entry(vm, attr) {
                Some(mut ev) => {
                    // resolveRefs=true resolves a @ref to the target's
                    // own type/value (e.g. a @color yields a color int).
                    // resourceId keeps pointing at the resource the
                    // final value came from (AOSP getResourceValue).
                    let mut res_id = 0;
                    for _ in 0..8 {
                        if !resolve_refs || ev.data_type != 0x01 {
                            break;
                        }
                        res_id = ev.data;
                        match vm.apk_res.as_ref().and_then(|r| r.entry_value(ev.data)) {
                            Some(next) => ev = next.clone(),
                            None => break,
                        }
                    }
                    if let Some(tv) = args.get(2).copied() {
                        fill_typed_value_ev(vm, tv, &ev, res_id);
                    }
                    Some(Some(V::I32(1)))
                }
                None => Some(Some(V::I32(0))),
            }
        }
        (
            "Landroid/content/res/Resources$Theme;",
            "applyStyle" | "setTo" | "rebase" | "dump" | "getKey" | "newTheme",
        ) => Some(None),
        ("Landroid/content/res/Configuration;", "isNightModeActive") => {
            // fossify's isSystemInDarkMode → this; answer from the
            // uiMode `getConfiguration` minted (default dark).
            let night = match args.first() {
                Some(V::Ref(ch)) => match pseudo_get(vm, *ch, crate::P_META) {
                    Some(V::I32(m)) => m & 0x30 == 0x20,
                    _ => true,
                },
                _ => true,
            };
            Some(Some(V::I32(if night { 1 } else { 0 })))
        }
        (
            "Landroid/content/res/Configuration;",
            "setTo"
            | "updateFrom"
            | "diff"
            | "isLayoutSizeAtLeast"
            | "setLayoutDirection"
            | "setLocale"
            | "setLocales"
            | "readFromProto"
            | "writeToProto"
            | "describeContents"
            | "writeToParcel"
            | "getLayoutDirection"
            | "isScreenWideColorGamut"
            | "isScreenHdr"
            | "isScreenRound"
            | "getLocales"
            | "getGrammaticalGender",
        ) => match name {
            "getLocales" => Some(Some(host(vm, "Landroid/os/LocaleList;", HostState::Empty))),
            // AOSP updateFrom returns the diff bitmask (int), not void —
            // a void result here breaks the dex call site.
            _ if name.starts_with("is")
                || name == "getLayoutDirection"
                || name == "diff"
                || name == "updateFrom"
                || name == "getGrammaticalGender" =>
            {
                Some(Some(V::I32(0)))
            }
            _ => Some(None),
        },
        ("Landroid/os/LocaleList;", _) => match name {
            "get" | "getFirstMatch" => Some(Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("en-US".into()),
            ))),
            "size" => Some(Some(V::I32(1))),
            "isEmpty" => Some(Some(V::I32(0))),
            "indexOf" => Some(Some(V::I32(0))),
            "getDefault" | "getAdjustedDefault" | "setDefault" => {
                Some(Some(host(vm, "Landroid/os/LocaleList;", HostState::Empty)))
            }
            "toString" | "toLanguageTags" => Some(Some(jstr(vm, "en-US"))),
            "matchesLanguageAndScript" => {
                let lang = |v: V| {
                    let V::Ref(h) = v else {
                        return String::new();
                    };
                    super::io::locale_tag(vm, h)
                        .split(['-', '_'])
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                };
                let script = |v: V| {
                    let V::Ref(h) = v else {
                        return String::new();
                    };
                    super::io::locale_tag(vm, h)
                        .split(['-', '_'])
                        .skip(1)
                        .find(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic()))
                        .unwrap_or("")
                        .to_ascii_lowercase()
                };
                let la = lang(args.first().copied().unwrap_or(V::Ref(0)));
                let lb = lang(args.get(1).copied().unwrap_or(V::Ref(0)));
                let sa = script(args.first().copied().unwrap_or(V::Ref(0)));
                let sb = script(args.get(1).copied().unwrap_or(V::Ref(0)));
                Some(Some(V::I32(
                    ((la.is_empty() || lb.is_empty() || la == lb)
                        && (sa.is_empty() || sb.is_empty() || sa == sb)) as i32,
                )))
            }
            _ => Some(None),
        },
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Handler / Looper — run posted work inline (single looper model)

fn handler_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "post"
        | "postDelayed"
        | "postAtTime"
        | "postAtFrontOfQueue"
        | "postAtTimeWithDelivery"
        | "sendMessage"
        | "sendMessageDelayed"
        | "sendMessageAtTime"
        | "sendMessageAtFrontOfQueue"
        | "sendEmptyMessage"
        | "sendEmptyMessageDelayed"
        | "sendEmptyMessageAtTime"
        | "postWithDeliveryResult"
        | "postAsIdle"
        | "executeOrSendMessage"
        | "sendToTarget" => {
            if let Some(V::Ref(r)) = args.get(1).copied() {
                // Handler posts land on the main looper — deferred so a
                // re-posting callback can't spin inside its own frame.
                // For sendMessage* the posted object is a Message —
                // stamp this handler as its target so the drain-time
                // dispatch can route it back here.
                if name.starts_with("send") && is_message(vm, r) {
                    if let Some(&V::Ref(h)) = args.first() {
                        pseudo_set(vm, r, P_MSG_TARGET, V::Ref(h));
                    }
                }
                // *Delayed takes (r, delayMs); *AtTime takes (r, when) —
                // our clocks are epoch-ms so `when - now` is the delay.
                let delay = if name.contains("Delayed") {
                    args.get(2).copied().map(i64v).unwrap_or(0)
                } else if name.contains("AtTime") {
                    args.get(2).copied().map(i64v).unwrap_or(0) - now_ms()
                } else {
                    0
                };
                if delay > 0 {
                    vm.enqueue_runnable_delayed(r, true, delay);
                } else {
                    vm.enqueue_runnable(r, true);
                }
            }
            Some(Some(V::I32(1)))
        }
        "handleMessage" | "dispatchMessage" | "callback" => {
            if let V::Ref(h) = args[0] {
                // `call_virtual_rt` — no builtin fallback: `call_virtual`
                // would route a non-overridden handleMessage back here
                // and recurse until the stack blows.
                let v = vm.call_virtual_rt(h, "handleMessage", &args[1..])?;
                return Ok(Some(v));
            }
            Some(None)
        }
        "removeCallbacks"
        | "removeMessages"
        | "removeCallbacksAndMessages"
        | "removeEqualMessages" => Some(None),
        "hasMessages" | "hasCallbacks" | "hasEqualMessages" => Some(Some(V::I32(0))),
        "obtainMessage" => {
            // obtainMessage(what[, obj|arg1,arg2,obj]) — target is `this`.
            let m = host(vm, "Landroid/os/Message;", HostState::Empty);
            if let (V::Ref(mh), Some(&V::Ref(h))) = (m, args.first()) {
                pseudo_set(vm, mh, P_MSG_TARGET, V::Ref(h));
                let mut ints = args
                    .iter()
                    .skip(1)
                    .filter(|a| matches!(a, V::I32(_) | V::I64(_)));
                if let Some(&w) = ints.next() {
                    pseudo_set(vm, mh, P_MSG_WHAT, w);
                }
                if let Some(&a1) = ints.next() {
                    pseudo_set(vm, mh, P_MSG_ARG1, a1);
                }
                if let Some(&a2) = ints.next() {
                    pseudo_set(vm, mh, P_MSG_ARG2, a2);
                }
                if let Some(&V::Ref(o)) = args.iter().skip(1).find(|a| matches!(a, V::Ref(_))) {
                    pseudo_set(vm, mh, P_MSG_OBJ, V::Ref(o));
                }
            }
            Some(Some(m))
        }
        "<init>" => {
            // <init>(Looper?, Callback?, boolean?) — remember the
            // bound looper and Handler.Callback so dispatch routes
            // handleMessage there.
            if let Some(&V::Ref(this)) = args.first() {
                for a in &args[1..] {
                    let V::Ref(h) = a else { continue };
                    let is_looper = matches!(
                        vm.heap.get(*h),
                        Some(Obj::Host { desc, .. }) if desc == "Landroid/os/Looper;"
                    );
                    if is_looper {
                        pseudo_set(vm, this, P_HANDLER_LOOPER, V::Ref(*h));
                    } else if *h != 0 {
                        pseudo_set(vm, this, P_HANDLER_CB, V::Ref(*h));
                        break;
                    }
                }
            }
            Some(None)
        }
        "getLooper" => Some(Some(
            args.first()
                .and_then(|v| {
                    if let V::Ref(h) = v {
                        pseudo_get(vm, *h, P_HANDLER_LOOPER)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| main_looper(vm)),
        )),
        "getMessageName" | "getTraceName" => Some(Some(jstr(vm, ""))),
        "createAsync" => Some(Some(args[0])),
        _ => None,
    })
}

fn looper_call(vm: &mut Vm, name: &str, _args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "getMainLooper" => Some(Some(main_looper(vm))),
        "myLooper" => {
            // A background thread has no looper unless it prepared one —
            // Looper.myLooper()==getMainLooper() must be false on workers.
            if vm.cur_thread != 0 && vm.cur_thread != vm.main_thread_h {
                Some(Some(V::Ref(0)))
            } else {
                Some(Some(main_looper(vm)))
            }
        }
        "prepare" | "prepareMainLooper" => Some(None),
        "loop" | "quit" | "quitSafely" => Some(None),
        "isCurrentThread" => Some(Some(V::I32(
            (vm.cur_thread == 0 || vm.cur_thread == vm.main_thread_h) as i32,
        ))),
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
// android.graphics — Rect/Point/Color/Bitmap stubs

fn graphics_call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    // Canvas/Paint fall through to jdk::gfx's recording model — the
    // generic set*/draw* stubs below would silently eat every call.
    if matches!(
        class,
        "Landroid/graphics/Canvas;" | "Landroid/graphics/Paint;" | "Landroid/text/TextPaint;"
    ) {
        return Ok(None);
    }
    Ok(match (class, name) {
        ("Landroid/graphics/Color;", "argb") | ("Landroid/graphics/Color;", "rgb") => {
            let (a, r, g, b) = if name == "argb" && args.len() >= 4 {
                (i32v(args[0]), i32v(args[1]), i32v(args[2]), i32v(args[3]))
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
        (
            "Landroid/graphics/Rect;",
            "set" | "offset" | "offsetTo" | "contains" | "width" | "height" | "isEmpty" | "centerX"
            | "centerY" | "exactCenterX" | "exactCenterY" | "equals" | "setEmpty" | "<init>",
        ) => {
            let h = match args.first() {
                Some(V::Ref(h)) => *h,
                _ => 0,
            };
            let (l, t, r, b) = rect_edges(vm, h);
            Some(match name {
                "set" | "<init>" => {
                    // set(l,t,r,b) / set(Rect) or <init>(l,t,r,b) /
                    // <init>() / <init>(Rect).
                    let vals: Vec<i32> = args[1..]
                        .iter()
                        .filter(|a| matches!(a, V::I32(_)))
                        .map(|a| i32v(*a))
                        .collect();
                    let (l2, t2, r2, b2) = if vals.len() == 4 {
                        (vals[0], vals[1], vals[2], vals[3])
                    } else {
                        match args.get(1) {
                            Some(&V::Ref(src)) => rect_edges(vm, src),
                            _ => (0, 0, 0, 0),
                        }
                    };
                    rect_write(vm, h, l2, t2, r2, b2);
                    Some(V::Ref(0))
                }
                "setEmpty" => {
                    rect_write(vm, h, 0, 0, 0, 0);
                    Some(V::Ref(0))
                }
                "offset" => {
                    let dx = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
                    let dy = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
                    rect_write(vm, h, l + dx, t + dy, r + dx, b + dy);
                    Some(V::Ref(0))
                }
                "offsetTo" => {
                    let nl = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
                    let nt = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
                    rect_write(vm, h, nl, nt, r - l + nl, b - t + nt);
                    Some(V::Ref(0))
                }
                "width" => Some(V::I32(r - l)),
                "height" => Some(V::I32(b - t)),
                "isEmpty" => Some(V::I32((l >= r || t >= b) as i32)),
                "centerX" => Some(V::I32((l + r) >> 1)),
                "centerY" => Some(V::I32((t + b) >> 1)),
                "exactCenterX" => Some(V::F32((l + r) as f32 * 0.5)),
                "exactCenterY" => Some(V::F32((t + b) as f32 * 0.5)),
                "contains" => {
                    // contains(x, y) tests a point; contains(l, t, r, b)
                    // tests a whole rectangle.
                    let inside = if args.len() >= 5 {
                        let il = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
                        let it = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
                        let ir = i32v(args.get(3).copied().unwrap_or(V::I32(0)));
                        let ib = i32v(args.get(4).copied().unwrap_or(V::I32(0)));
                        l < r && t < b && l <= il && t <= it && r >= ir && b >= ib
                    } else {
                        let x = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
                        let y = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
                        l < r && t < b && x >= l && x < r && y >= t && y < b
                    };
                    Some(V::I32(inside as i32))
                }
                "equals" => {
                    let that = args.get(1).copied().unwrap_or(V::Ref(0));
                    let (l2, t2, r2, b2) = match that {
                        V::Ref(h2) => rect_edges(vm, h2),
                        _ => (0, 0, 0, 0),
                    };
                    Some(V::I32((l == l2 && t == t2 && r == r2 && b == b2) as i32))
                }
                _ => Some(V::Ref(0)),
            })
        }
        // `ShapeDrawable.getPaint()`: a real Paint object — callers
        // (SwipeRefreshLayout) immediately call setColor on it.
        (c, "getPaint") if c.ends_with("Drawable;") => match args.first() {
            Some(V::Ref(h)) => {
                if let Some(v @ V::Ref(_)) = pseudo_get(vm, *h, P_PAINT) {
                    Some(Some(v))
                } else {
                    let p = host(vm, "Landroid/graphics/Paint;", HostState::Empty);
                    pseudo_set(vm, *h, P_PAINT, p);
                    Some(Some(p))
                }
            }
            _ => Some(Some(V::Ref(0))),
        },
        // Drawable bounds: a cached Rect mirroring setBounds writes —
        // TabLayout's indicator path reads `drawable.getBounds().left`.
        (
            c,
            "getBounds" | "copyBounds" | "setBounds" | "mutate" | "getIntrinsicWidth"
            | "getIntrinsicHeight" | "invalidateSelf" | "setAlpha" | "setColorFilter"
            | "getOpacity" | "setVisible",
        ) if c.ends_with("Drawable;") => {
            let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
                return Ok(Some(None));
            };
            match name {
                "getBounds" => Some(Some(bounds_rect(vm, h))),
                "copyBounds" => {
                    if let Some(&V::Ref(dst)) = args.get(1) {
                        // copyBounds(Rect) — write the caller's rect, void.
                        let (l, t, r, b) = rect_edges(vm, bounds_h(vm, h).unwrap_or(0));
                        rect_write(vm, dst, l, t, r, b);
                        Some(None)
                    } else {
                        // copyBounds() — returns the bounds Rect.
                        Some(Some(bounds_rect(vm, h)))
                    }
                }
                "setBounds" => {
                    // (l,t,r,b) or (Rect)
                    let ints: Vec<i32> = args[1..]
                        .iter()
                        .filter(|a| matches!(a, V::I32(_)))
                        .map(|a| i32v(*a))
                        .collect();
                    let (l, t, r, b) = if ints.len() == 4 {
                        (ints[0], ints[1], ints[2], ints[3])
                    } else {
                        args.get(1)
                            .and_then(|a| {
                                if let V::Ref(rh) = a {
                                    Some(rect_edges(vm, *rh))
                                } else {
                                    None
                                }
                            })
                            .unwrap_or((0, 0, 0, 0))
                    };
                    // Create the cached rect when setBounds precedes
                    // getBounds — the first bounds write must land too.
                    let rh = match bounds_rect(vm, h) {
                        V::Ref(rh) => rh,
                        _ => 0,
                    };
                    rect_write(vm, rh, l, t, r, b);
                    Some(None)
                }
                "mutate" => Some(Some(V::Ref(h))),
                "getIntrinsicWidth" => {
                    // BitmapDrawable: intrinsic size is the bitmap's;
                    // bounds and -1 (unspecified) come after that.
                    if let Some((w, ..)) = bitmap_pixels(vm, V::Ref(h)) {
                        return Ok(Some(Some(V::I32(w as i32))));
                    }
                    let (l, _, r, _) = rect_edges(vm, bounds_h(vm, h).unwrap_or(0));
                    Some(Some(V::I32(r - l)))
                }
                "getIntrinsicHeight" => {
                    if let Some((_, hgt, _)) = bitmap_pixels(vm, V::Ref(h)) {
                        return Ok(Some(Some(V::I32(hgt as i32))));
                    }
                    let (_, t, _, b) = rect_edges(vm, bounds_h(vm, h).unwrap_or(0));
                    Some(Some(V::I32(b - t)))
                }
                "getOpacity" => Some(Some(V::I32(-3))), // TRANSLUCENT
                "setVisible" => Some(Some(V::I32(0))),
                _ => Some(None),
            }
        }
        // `Drawable.draw(Canvas)` — guest views (ImageView/FAB onDraw)
        // paint their drawable through this call; it must emit a bitmap
        // op into the recording canvas or the icon is invisible.
        (c, "draw") if c.ends_with("Drawable;") => {
            let Some(canvas) = args.get(1).copied() else {
                return Ok(Some(None));
            };
            let px = bitmap_pixels(vm, args[0]);
            if crate::dbg_flag("OVELO_DRAWDBG") {
                eprintln!(
                    "[drawdbg] {c}.draw px={} canvas_arg={:?}",
                    px.as_ref()
                        .map(|(w, h, _)| format!("{w}x{h}"))
                        .unwrap_or_else(|| "-".into()),
                    args.get(1)
                );
            }
            // ColorDrawable: fill the drawable's bounds.
            if px.is_none() {
                if let (V::Ref(dh), Some(V::I32(argb))) = (
                    args.first().copied().unwrap_or(V::I32(0)),
                    args.first().and_then(|a| match a {
                        V::Ref(h) => pseudo_get(vm, *h, P_DRAW_COLOR),
                        _ => None,
                    }),
                ) {
                    let (l, t, r, b) = rect_edges(vm, bounds_h(vm, dh).unwrap_or(0));
                    if r > l && b > t {
                        gfx::rec(
                            vm,
                            canvas,
                            CanvasOp::Rect {
                                l: l as f32,
                                t: t as f32,
                                r: r as f32,
                                b: b as f32,
                                paint: PaintState {
                                    argb,
                                    ..PaintState::default()
                                },
                            },
                        );
                    }
                }
            }
            if let Some((sw, sh, rgba)) = px {
                let rect_h = args
                    .first()
                    .and_then(|a| match a {
                        V::Ref(h) => bounds_h(vm, *h),
                        _ => None,
                    })
                    .unwrap_or(0);
                let (l, t, r, b) = rect_edges(vm, rect_h);
                let (x, y, dw, dh) = if r > l && b > t {
                    (l as f32, t as f32, (r - l) as f32, (b - t) as f32)
                } else {
                    (0.0, 0.0, sw as f32, sh as f32)
                };
                gfx::rec(
                    vm,
                    canvas,
                    CanvasOp::Bitmap {
                        x,
                        y,
                        w: dw.max(0.0) as u32,
                        h: dh.max(0.0) as u32,
                        src_w: sw,
                        src_h: sh,
                        rgba,
                        paint: crate::PaintState::default(),
                    },
                );
            }
            Some(None)
        }
        ("Landroid/graphics/Region;", n) => {
            let this = match args.first() {
                Some(&V::Ref(h)) => h,
                _ => 0,
            };
            // Region is tracked as one bounding rect — enough for the
            // view/contentcapture traversal paths that ask getBounds.
            let bounds = |vm: &Vm, h: u32| rect_edges(vm, h);
            Some(match n {
                "<init>" => {
                    let ints: Vec<i32> = args[1..]
                        .iter()
                        .filter(|a| matches!(a, V::I32(_)))
                        .map(|a| i32v(*a))
                        .collect();
                    let (l, t, r, b) = if ints.len() == 4 {
                        (ints[0], ints[1], ints[2], ints[3])
                    } else {
                        match args.get(1) {
                            Some(&V::Ref(src)) => rect_edges(vm, src),
                            _ => (0, 0, 0, 0),
                        }
                    };
                    rect_write(vm, this, l, t, r, b);
                    None
                }
                "set" | "setPath" | "union" | "freeStorage" => {
                    let ints: Vec<i32> = args[1..]
                        .iter()
                        .filter(|a| matches!(a, V::I32(_)))
                        .map(|a| i32v(*a))
                        .collect();
                    let (l, t, r, b) = if ints.len() == 4 {
                        (ints[0], ints[1], ints[2], ints[3])
                    } else {
                        match args.get(1) {
                            Some(&V::Ref(src)) => rect_edges(vm, src),
                            _ => (0, 0, 0, 0),
                        }
                    };
                    if n == "union" {
                        let (l0, t0, r0, b0) = bounds(vm, this);
                        if l0 < r0 && t0 < b0 {
                            rect_write(vm, this, l0.min(l), t0.min(t), r0.max(r), b0.max(b));
                        } else {
                            rect_write(vm, this, l, t, r, b);
                        }
                    } else if n != "freeStorage" {
                        rect_write(vm, this, l, t, r, b);
                    }
                    if n == "set" || n == "union" {
                        Some(V::I32(1))
                    } else {
                        None
                    }
                }
                "setEmpty" => {
                    rect_write(vm, this, 0, 0, 0, 0);
                    Some(V::I32(1))
                }
                "op" => {
                    // op(Rect|Region, Op) — Op: DIFFERENCE=0, INTERSECT=1,
                    // UNION=2, XOR=3, REVERSE_DIFFERENCE=4, REPLACE=5.
                    let op = args.last().map(|v| i64v(*v)).unwrap_or(2);
                    let (src_l, src_t, src_r, src_b) = match args.get(1) {
                        Some(&V::Ref(src)) => rect_edges(vm, src),
                        _ => (0, 0, 0, 0),
                    };
                    let (l, t, r, b) = bounds(vm, this);
                    let empty = !(l < r && t < b);
                    let mut ok = true;
                    match op {
                        1 => {
                            // INTERSECT
                            if empty || src_l >= src_r || src_t >= src_b {
                                rect_write(vm, this, 0, 0, 0, 0);
                                ok = false;
                            } else {
                                rect_write(
                                    vm,
                                    this,
                                    l.max(src_l),
                                    t.max(src_t),
                                    r.min(src_r),
                                    b.min(src_b),
                                );
                                let (nl, nt, nr, nb) = bounds(vm, this);
                                ok = nl < nr && nt < nb;
                            }
                        }
                        2 | 3 => {
                            // UNION / XOR ≈ union of bounds.
                            if empty {
                                rect_write(vm, this, src_l, src_t, src_r, src_b);
                            } else {
                                rect_write(
                                    vm,
                                    this,
                                    l.min(src_l),
                                    t.min(src_t),
                                    r.max(src_r),
                                    b.max(src_b),
                                );
                            }
                        }
                        5 => {
                            // REPLACE
                            rect_write(vm, this, src_l, src_t, src_r, src_b);
                        }
                        _ => {} // DIFFERENCE / REVERSE_DIFFERENCE ≈ no-op
                    }
                    Some(V::I32(ok as i32))
                }
                "getBounds" => {
                    let (l, t, r, b) = bounds(vm, this);
                    if let Some(&V::Ref(out)) = args.get(1) {
                        rect_write(vm, out, l, t, r, b);
                        Some(V::I32((l < r && t < b) as i32))
                    } else {
                        let rh = host(vm, "Landroid/graphics/Rect;", HostState::Empty);
                        if let V::Ref(h) = rh {
                            rect_write(vm, h, l, t, r, b);
                        }
                        Some(rh)
                    }
                }
                "isEmpty" => {
                    let (l, t, r, b) = bounds(vm, this);
                    Some(V::I32(!(l < r && t < b) as i32))
                }
                "contains" | "quickContains" => {
                    let (l, t, r, b) = bounds(vm, this);
                    let x = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
                    let y = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
                    Some(V::I32((x >= l && x < r && y >= t && y < b) as i32))
                }
                "quickReject" => {
                    let (l, t, r, b) = bounds(vm, this);
                    let (sl, st, sr, sb) = match args.get(1) {
                        Some(&V::Ref(src)) => rect_edges(vm, src),
                        _ => (0, 0, 0, 0),
                    };
                    Some(V::I32((sr <= l || sl >= r || sb <= t || st >= b) as i32))
                }
                "translate" => {
                    let dx = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
                    let dy = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
                    let (l, t, r, b) = bounds(vm, this);
                    rect_write(vm, this, l + dx, t + dy, r + dx, b + dy);
                    if args.len() > 3 {
                        if let Some(&V::Ref(dst)) = args.get(3) {
                            rect_write(vm, dst, l + dx, t + dy, r + dx, b + dy);
                        }
                    }
                    None
                }
                "isComplex" | "isRect" | "isConvex" => {
                    let (l, t, r, b) = bounds(vm, this);
                    Some(V::I32((n != "isComplex" && l < r && t < b) as i32))
                }
                "getBoundaryPath" => Some(V::I32(0)),
                "describeContents" | "writeToParcel" | "equals" | "hashCode" => Some(V::I32(0)),
                _ => return Ok(None),
            })
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
        (
            "Landroid/util/AttributeSet;",
            "getAttributeValue"
            | "getAttributeName"
            | "getAttributeResourceValue"
            | "getAttributeIntValue"
            | "getAttributeBooleanValue"
            | "getAttributeFloatValue"
            | "getAttributeCount"
            | "getAttributeNameResource"
            | "getPositionDescription"
            | "getIdAttribute"
            | "getIdAttributeResourceValue"
            | "getClassAttribute"
            | "getStyleAttribute"
            | "getAttributeUnsignedIntValue",
        ) => {
            // An XmlResourceParser receiver IS an AttributeSet too —
            // reads follow its cursor, not this generic stub.
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host {
                    state: HostState::Xml { .. },
                    ..
                }) = vm.heap.get(*h)
                {
                    return xml::pull_call(vm, class, name, "()V", args);
                }
            }
            if name.contains("Count")
                || name.contains("Resource")
                || name.contains("Int")
                || name == "getStyleAttribute"
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
        ("Landroid/util/Size;", "getWidth" | "getHeight") => Some(Some(V::I32(0))),
        ("Landroid/graphics/drawable/BitmapDrawable;", "<init>") => {
            // Keep the Bitmap arg discoverable: the renderer walks an
            // ImageView's drawable fields for pixels (and getBitmap
            // reads them back). Signatures: (Resources, Bitmap),
            // (Bitmap), (Resources, Bitmap, Paint), (Resources,
            // DisplayMetrics, Bitmap, Paint).
            if let Some(V::Ref(this)) = args.first() {
                let bm = args[1..]
                    .iter()
                    .find(|a| bitmap_dims(vm, **a) != (0, 0))
                    .copied();
                if let Some(bm) = bm {
                    pseudo_set(vm, *this, P_BITMAP_SLOT, bm);
                }
            }
            Some(None)
        }
        ("Landroid/graphics/drawable/LayerDrawable;", "<init>")
        | ("Landroid/graphics/drawable/TransitionDrawable;", "<init>") => {
            // Keep the `Drawable[]` layers arg discoverable: the
            // renderer's bitmap field-walk descends into it to reach
            // the wrapped drawables (Glide fades through a
            // TransitionDrawable).
            if let Some(V::Ref(this)) = args.first() {
                let layers = args[1..]
                    .iter()
                    .find(|a| {
                        matches!(a, V::Ref(h)
                            if matches!(vm.heap.get(*h), Some(Obj::Array { .. })))
                    })
                    .copied();
                if let Some(l) = layers {
                    pseudo_set(vm, *this, P_LAYERS, l);
                }
            }
            Some(None)
        }
        ("Landroid/graphics/drawable/BitmapDrawable;", "getBitmap") => {
            Some(Some(match args.first() {
                Some(V::Ref(h)) => pseudo_get(vm, *h, P_BITMAP_SLOT).unwrap_or(V::Ref(0)),
                _ => V::Ref(0),
            }))
        }
        ("Landroid/graphics/BitmapFactory;", "decodeFile") => {
            let path = sv(vm, args[0]).unwrap_or_default();
            let bytes = std::fs::read(io::fspath(vm, &path)).unwrap_or_default();
            Some(bitmap_decode_opts(vm, &bytes, args))
        }
        ("Landroid/graphics/BitmapFactory;", "decodeByteArray") => {
            let data = match args.first().copied() {
                Some(V::Ref(h)) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => {
                        data.iter().map(|v| i32v(*v) as u8).collect::<Vec<u8>>()
                    }
                    _ => vec![],
                },
                _ => vec![],
            };
            let off = i32v(args.get(1).copied().unwrap_or(V::I32(0))).max(0) as usize;
            let len =
                i32v(args.get(2).copied().unwrap_or(V::I32(data.len() as i32))).max(0) as usize;
            let bytes = &data[off.min(data.len())..(off + len).min(data.len())];
            Some(bitmap_decode_opts(vm, bytes, args))
        }
        ("Landroid/graphics/BitmapFactory;", "decodeStream") => {
            let bytes = match args.first().copied() {
                Some(V::Ref(h)) => stream_bytes(vm, h),
                _ => vec![],
            };
            Some(bitmap_decode_opts(vm, &bytes, args))
        }
        ("Landroid/graphics/BitmapFactory;", "decodeFileDescriptor") => {
            let bytes = match args.first().copied() {
                Some(V::Ref(h)) => stream_bytes(vm, h),
                _ => vec![],
            };
            Some(bitmap_decode_opts(vm, &bytes, args))
        }
        ("Landroid/graphics/BitmapFactory;", "decodeResource") => {
            // (Resources, resId): vector drawables rasterize too, then
            // fall back to the packaged file's PNG/JPEG decode.
            let res_id = i32v(*args.get(1).unwrap_or(args.first().unwrap_or(&V::I32(0)))) as u32;
            Some(
                match super::vector::rctx(vm)
                    .and_then(|r| super::vector::drawable_bitmap(r, res_id))
                {
                    Some((w, h, rgba)) => Some(V::Ref(vm.alloc_host(
                        "Landroid/graphics/Bitmap;",
                        HostState::Bitmap { w, h, rgba },
                    ))),
                    None => {
                        let bytes = vm
                            .apk_res
                            .as_ref()
                            .and_then(|r| r.file_path(res_id).and_then(|p| r.read(p)))
                            .unwrap_or_default();
                        bitmap_decode(vm, &bytes)
                    }
                },
            )
        }
        (
            "Landroid/graphics/Bitmap;",
            "getWidth" | "getHeight" | "getByteCount" | "getRowBytes" | "hasAlpha" | "isRecycled"
            | "getDensity" | "getGenerationId",
        ) => {
            let (w, h) = bitmap_dims(vm, args[0]);
            Some(Some(match name {
                "getWidth" => V::I32(w as i32),
                "getHeight" => V::I32(h as i32),
                "getByteCount" => V::I32((w * h * 4) as i32),
                "getRowBytes" => V::I32((w * 4) as i32),
                "getDensity" => V::I32(160),
                "isRecycled" => V::I32(0),
                _ => V::I32(1), // hasAlpha / getGenerationId
            }))
        }
        ("Landroid/graphics/Bitmap;", "getPixels") => {
            // getPixels(int[] dst, int offset, int stride, int x, int y, int w, int h)
            Some(Some({
                if let Some((bw, bh, rgba)) = bitmap_pixels(vm, args[0]) {
                    if let V::Ref(arr) = args.get(1).copied().unwrap_or(V::I32(0)) {
                        let (off, stride, x0, y0, w, h) = (
                            i32v(args.get(2).copied().unwrap_or(V::I32(0))).max(0) as usize,
                            i32v(args.get(3).copied().unwrap_or(V::I32(0))).max(0) as usize,
                            i32v(args.get(4).copied().unwrap_or(V::I32(0))).max(0) as u32,
                            i32v(args.get(5).copied().unwrap_or(V::I32(0))).max(0) as u32,
                            i32v(args.get(6).copied().unwrap_or(V::I32(0))).max(0) as u32,
                            i32v(args.get(7).copied().unwrap_or(V::I32(0))).max(0) as u32,
                        );
                        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(arr) {
                            for y in 0..h.min(bh.saturating_sub(y0)) {
                                for x in 0..w.min(bw.saturating_sub(x0)) {
                                    let s = (((y0 + y) * bw + (x0 + x)) * 4) as usize;
                                    // Color ints are 0xAARRGGBB.
                                    let v = (rgba[s + 3] as i32) << 24
                                        | (rgba[s] as i32) << 16
                                        | (rgba[s + 1] as i32) << 8
                                        | rgba[s + 2] as i32;
                                    let d = off + y as usize * stride + x as usize;
                                    if let Some(slot) = data.get_mut(d) {
                                        *slot = V::I32(v);
                                    }
                                }
                            }
                        }
                    }
                }
                args[0]
            }))
        }
        ("Landroid/graphics/Bitmap;", "setPixels") => {
            // setPixels(int[] src, int offset, int stride, int x, int y, int w, int h)
            Some(Some({
                if let V::Ref(bh) = args[0] {
                    if let (
                        Some(Obj::Host {
                            state:
                                HostState::Bitmap {
                                    w: bw,
                                    h: bh2,
                                    rgba,
                                },
                            ..
                        }),
                        V::Ref(arr),
                    ) = (vm.heap.get(bh), args.get(1).copied().unwrap_or(V::I32(0)))
                    {
                        if let Some(Obj::Array { data, .. }) = vm.heap.get(arr) {
                            let src: Vec<i32> = data.iter().map(|v| i32v(*v)).collect();
                            let (off, stride, x0, y0, w, h) = (
                                i32v(args.get(2).copied().unwrap_or(V::I32(0))).max(0) as usize,
                                i32v(args.get(3).copied().unwrap_or(V::I32(0))).max(0) as usize,
                                i32v(args.get(4).copied().unwrap_or(V::I32(0))).max(0) as u32,
                                i32v(args.get(5).copied().unwrap_or(V::I32(0))).max(0) as u32,
                                i32v(args.get(6).copied().unwrap_or(V::I32(0))).max(0) as u32,
                                i32v(args.get(7).copied().unwrap_or(V::I32(0))).max(0) as u32,
                            );
                            let mut px = rgba.as_ref().clone();
                            for y in 0..h.min(bh2.saturating_sub(y0)) {
                                for x in 0..w.min(bw.saturating_sub(x0)) {
                                    let v = src
                                        .get(off + y as usize * stride + x as usize)
                                        .copied()
                                        .unwrap_or(0)
                                        as u32;
                                    let d = (((y0 + y) * bw + (x0 + x)) * 4) as usize;
                                    px[d] = ((v >> 16) & 0xFF) as u8;
                                    px[d + 1] = ((v >> 8) & 0xFF) as u8;
                                    px[d + 2] = (v & 0xFF) as u8;
                                    px[d + 3] = ((v >> 24) & 0xFF) as u8;
                                }
                            }
                            if let Some(Obj::Host {
                                state: HostState::Bitmap { rgba: dst, .. },
                                ..
                            }) = vm.heap.get_mut(bh)
                            {
                                *dst = std::rc::Rc::new(px);
                            }
                        }
                    }
                }
                args[0]
            }))
        }
        ("Landroid/graphics/Bitmap;", "copyPixelsToBuffer") => {
            // copyPixelsToBuffer(Buffer dst): writes packed ARGB ints —
            // byte order is CPU-native (LE), giving R,G,B,A byte lanes
            // which is what GL_RGBA/ubyte expects on upload.
            Some(Some({
                if let (Some((bw, bh, rgba)), V::Ref(dh)) = (
                    bitmap_pixels(vm, args[0]),
                    args.get(1).copied().unwrap_or(V::I32(0)),
                ) {
                    if let Some(Obj::Host {
                        state: HostState::Slot(V::Ref(arr)),
                        ..
                    }) = vm.heap.get(dh)
                    {
                        let esz = i32v(pseudo_get(vm, dh, super::P_BBUF_ESZ).unwrap_or(V::I32(1)))
                            .clamp(1, 8) as usize;
                        let pos = i32v(pseudo_get(vm, dh, super::P_BBUF_POS).unwrap_or(V::I32(0)))
                            .max(0) as usize;
                        let base = i32v(pseudo_get(vm, dh, super::P_BBUF_BASE).unwrap_or(V::I32(0)))
                            .max(0) as usize;
                        let mut d = base + pos * esz;
                        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(*arr) {
                            let n = ((bw * bh) as usize).min(rgba.len() / 4);
                            for p in 0..n {
                                for k in 0..4 {
                                    if let Some(slot) = data.get_mut(d + k) {
                                        *slot = V::I32(rgba[p * 4 + k] as i8 as i32);
                                    }
                                }
                                d += 4;
                            }
                            pseudo_set(
                                vm,
                                dh,
                                super::P_BBUF_POS,
                                V::I32(pos as i32 + ((bw * bh * 4) / esz as u32) as i32),
                            );
                        }
                    }
                }
                args[0]
            }))
        }
        ("Landroid/graphics/Bitmap;", "copyPixelsFromBuffer") => Some(Some({
            if let (V::Ref(bh), V::Ref(dh)) = (args[0], args.get(1).copied().unwrap_or(V::I32(0))) {
                if let (
                    Some(Obj::Host {
                        state:
                            HostState::Bitmap {
                                w: bw,
                                h: bh2,
                                rgba,
                            },
                        ..
                    }),
                    Some(Obj::Host {
                        state: HostState::Slot(V::Ref(arr)),
                        ..
                    }),
                ) = (vm.heap.get(bh), vm.heap.get(dh))
                {
                    let esz = i32v(pseudo_get(vm, dh, super::P_BBUF_ESZ).unwrap_or(V::I32(1)))
                        .clamp(1, 8) as usize;
                    let pos = i32v(pseudo_get(vm, dh, super::P_BBUF_POS).unwrap_or(V::I32(0)))
                        .max(0) as usize;
                    let base = i32v(pseudo_get(vm, dh, super::P_BBUF_BASE).unwrap_or(V::I32(0)))
                        .max(0) as usize;
                    let mut px = rgba.as_ref().clone();
                    if let Some(Obj::Array { data, .. }) = vm.heap.get(*arr) {
                        let n = ((bw * bh2) as usize).min(px.len() / 4);
                        let mut s = base + pos * esz;
                        for p in 0..n {
                            for k in 0..4 {
                                px[p * 4 + k] =
                                    data.get(s + k).map(|v| i32v(*v) as u8).unwrap_or(0);
                            }
                            s += 4;
                        }
                    }
                    if let Some(Obj::Host {
                        state: HostState::Bitmap { rgba: dst, .. },
                        ..
                    }) = vm.heap.get_mut(bh)
                    {
                        *dst = std::rc::Rc::new(px);
                    }
                }
            }
            args[0]
        })),
        ("Landroid/graphics/Bitmap;", "createBitmap" | "createScaledBitmap") => {
            // createBitmap(w, h, Config, ...) / createScaledBitmap(src, w, h, filter)
            let (w, h, src) = if name == "createScaledBitmap" {
                (
                    i32v(args.get(1).copied().unwrap_or(V::I32(0))).max(0) as u32,
                    i32v(args.get(2).copied().unwrap_or(V::I32(0))).max(0) as u32,
                    Some(args[0]),
                )
            } else {
                // API 26 adds a leading nullable DisplayMetrics to the
                // overload Compose uses; static builtins receive every
                // real parameter (there is no implicit `this`).
                let shift = usize::from(sig.starts_with("(Landroid/util/DisplayMetrics;"));
                (
                    i32v(args.get(shift).copied().unwrap_or(V::I32(0))).max(0) as u32,
                    i32v(args.get(shift + 1).copied().unwrap_or(V::I32(0))).max(0) as u32,
                    None,
                )
            };
            let rgba = match src.and_then(|s| bitmap_pixels(vm, s)) {
                Some((sw, sh, px)) => scale_rgba(sw, sh, &px, w, h),
                None => vec![0u8; (w * h * 4) as usize],
            };
            Some(Some(host(
                vm,
                "Landroid/graphics/Bitmap;",
                HostState::Bitmap {
                    w,
                    h,
                    rgba: std::rc::Rc::new(rgba),
                },
            )))
        }
        ("Landroid/util/SizeF;", "getWidth" | "getHeight") => Some(Some(V::F32(0.0))),
        (
            _,
            "createBitmap"
            | "createScaledBitmap"
            | "decodeResource"
            | "decodeFile"
            | "decodeStream"
            | "decodeByteArray"
            | "decodeFileDescriptor"
            | "inflate"
            | "getDrawable"
            | "obtain"
            | "obtainStyledAttributes",
        ) => {
            if name == "inflate" {
                Some(Some(host(
                    vm,
                    "Landroid/content/res/TypedArray;",
                    HostState::Empty,
                )))
            } else if name == "obtainStyledAttributes" {
                Some(Some(view::typed_array(vm, args)))
            } else {
                Some(Some(host(vm, class, HostState::Empty)))
            }
        }
        // Void-only fallbacks — answering `void` for a fluent setter
        // (LineBreakConfig$Builder.setLineBreakStyle → Builder) makes the
        // caller's move-result blow up with `dexrt.result.void`, which
        // unwinds past guest try/finally and leaves Compose's
        // duringMeasureLayout flag stuck. Non-void misses fall through
        // to the proto-typed stub instead.
        (_, n) if (n.starts_with("set") || n.starts_with("draw")) && sig.ends_with(")V") => {
            Some(None)
        }
        (_, n)
            if (n.starts_with("get") || n.starts_with("is") || n.starts_with("has"))
                && (sig.ends_with(")Z") || sig.ends_with(")I")) =>
        {
            Some(Some(V::I32(0)))
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Rect / Drawable bounds helpers

/// `Rect`'s four edges. A guest `rect.left = x` iput lands in the
/// object's canon field map — read it first, pseudo ints second, so
/// methods and direct field access never disagree.
pub(crate) fn rect_edges(vm: &Vm, h: u32) -> (i32, i32, i32, i32) {
    let read = |name: &str, key: u32| -> i32 {
        if let Some(f) = vm
            .field_index_of("Landroid/graphics/Rect;", name)
            .map(|g| vm.canon_field(g))
        {
            if let Some(Obj::Instance { fields, .. } | Obj::Host { fields, .. }) = vm.heap.get(h) {
                if let Some(v) = fields.get(&f) {
                    return i32v(*v);
                }
            }
        }
        i32v(pseudo_get(vm, h, key).unwrap_or(V::I32(0)))
    };
    (
        read("left", P_RECT_L),
        read("top", P_RECT_T),
        read("right", P_RECT_R),
        read("bottom", P_RECT_B),
    )
}

pub(crate) fn rect_write(vm: &mut Vm, h: u32, l: i32, t: i32, r: i32, b: i32) {
    pseudo_set(vm, h, P_RECT_L, V::I32(l));
    pseudo_set(vm, h, P_RECT_T, V::I32(t));
    pseudo_set(vm, h, P_RECT_R, V::I32(r));
    pseudo_set(vm, h, P_RECT_B, V::I32(b));
    // Mirror into the canon field map so a following `iget rect.left`
    // sees the method-side write too.
    for (name, v) in [("left", l), ("top", t), ("right", r), ("bottom", b)] {
        if let Some(f) = vm
            .field_index_of("Landroid/graphics/Rect;", name)
            .map(|g| vm.canon_field(g))
        {
            if let Some(Obj::Instance { fields, .. } | Obj::Host { fields, .. }) =
                vm.heap.get_mut(h)
            {
                fields.insert(f, V::I32(v));
            }
        }
    }
}

/// `Gravity.getAbsoluteGravity`: resolve START/END relative bits
/// against a layout direction (0 = LTR, 1 = RTL).
fn absolute_gravity(gravity: i32, dir: i32) -> i32 {
    let mut result = gravity;
    const RELATIVE_LAYOUT_DIRECTION: i32 = 0x0080_0000;
    const START: i32 = 0x0080_0003;
    const END: i32 = 0x0080_0005;
    const LEFT: i32 = 0x0000_0003;
    const RIGHT: i32 = 0x0000_0005;
    if result > 0 && (result & RELATIVE_LAYOUT_DIRECTION) == RELATIVE_LAYOUT_DIRECTION {
        result &= !RELATIVE_LAYOUT_DIRECTION;
        if dir == 1 {
            if (result & START) == START {
                result = (result & !START) | RIGHT;
            } else if (result & END) == END {
                result = (result & !END) | LEFT;
            }
        } else {
            if (result & START) == START {
                result = (result & !START) | LEFT;
            } else if (result & END) == END {
                result = (result & !END) | RIGHT;
            }
        }
    }
    result
}

/// `Gravity.apply`: place a `w`×`h` child inside `container` per the
/// gravity flags and write the resulting frame into `out`. Without
/// this the AOSP `GravityCompat.apply(container, out)` idiom leaves
/// `out` at (0,0,0,0) — every child then gets `layout(0,0,0,0)`.
#[allow(clippy::too_many_arguments)]
fn gravity_apply(
    vm: &mut Vm,
    gravity: i32,
    w: i32,
    h: i32,
    container: V,
    x_adj: i32,
    y_adj: i32,
    out: V,
    dir: i32,
) {
    let (V::Ref(ch), V::Ref(oh)) = (container, out) else {
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] gravity apply not-refs c={container:?} o={out:?}");
        }
        return;
    };
    if oh == 0 {
        return;
    }
    let (cl, ct, cr, cb) = rect_edges(vm, ch);
    const LEFT: i32 = 0x3;
    const RIGHT: i32 = 0x5;
    const CENTER_H: i32 = 0x1;
    const FILL_H: i32 = 0x7;
    const TOP: i32 = 0x30;
    const BOTTOM: i32 = 0x50;
    const CENTER_V: i32 = 0x10;
    const FILL_V: i32 = 0x70;
    let abs = absolute_gravity(gravity, dir);
    // Unset axes default to TOP|START per AOSP callers (NO_GRAVITY is
    // resolved upstream, but skeleton callers pass it straight in).
    let hg = match abs & FILL_H {
        0 => LEFT,
        m => m,
    };
    let vg = match gravity & FILL_V {
        0 => TOP,
        m => m,
    };
    let (l, r) = match hg {
        FILL_H => (cl + x_adj, cr - x_adj),
        RIGHT => (cr - x_adj - w, cr - x_adj),
        CENTER_H => {
            let lft = cl + (cr - cl - w) / 2 + x_adj;
            (lft, lft + w)
        }
        _ => (cl + x_adj, cl + x_adj + w),
    };
    let (t, b) = match vg {
        FILL_V => (ct + y_adj, cb - y_adj),
        BOTTOM => (cb - y_adj - h, cb - y_adj),
        CENTER_V => {
            let tp = ct + (cb - ct - h) / 2 + y_adj;
            (tp, tp + h)
        }
        _ => (ct + y_adj, ct + y_adj + h),
    };
    if crate::dbg_flag("OVELO_VIEWDBG") {
        eprintln!("[viewdbg] gravity apply h{oh} c=({cl},{ct},{cr},{cb}) -> ({l},{t},{r},{b})");
    }
    rect_write(vm, oh, l, t, r, b);
}

/// `ApplicationInfo` with the path fields populated — guests read
/// `dataDir`/`sourceDir`/`nativeLibraryDir` directly (Korgw seeds its
/// VFS root from `dataDir`) and null paths poison downstream caches.
fn app_info(vm: &mut Vm) -> V {
    let h = match host(vm, "Landroid/content/pm/ApplicationInfo;", HostState::Empty) {
        V::Ref(h) => h,
        _ => unreachable!(),
    };
    let pkg = vm.guest_package.clone().unwrap_or_else(|| "app".into());
    let data = jstr(vm, &format!("/data/data/{pkg}"));
    let src = jstr(vm, &format!("/data/app/{pkg}/base.apk"));
    let libs = jstr(vm, &format!("/data/app/{pkg}/lib/arm64"));
    let pkg_v = jstr(vm, &pkg);
    pseudo_set(vm, h, P_COMPONENT, pkg_v);
    pseudo_set(vm, h, P_PROCESS, pkg_v);
    pseudo_set(vm, h, P_DATA, data);
    pseudo_set(vm, h, P_META, src);
    pseudo_set(vm, h, P_FLAGS, libs);
    // ApplicationInfo.name is the Application subclass from the
    // manifest (null when undeclared) — NOT the package name; guests
    // Class.forName it to reach their app class.
    if let Some(cls) = vm.guest_app_class.clone() {
        let cv = jstr(vm, &cls);
        pseudo_set(vm, h, crate::jdk::P_APPCLASS, cv);
    }
    V::Ref(h)
}

/// `SharedPreferences` host object interned per file name — Android
/// prefs are file-backed, so every getter for one file must share a
/// map (a fresh empty map per call silently dropped every write).
fn xml_unescape(s: &str) -> String {
    s.replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
}

fn pref_name(line: &str) -> Option<String> {
    line.split("name=\"")
        .nth(1)
        .and_then(|x| x.split('\"').next())
        .map(xml_unescape)
}

fn pref_strings(line: &str) -> Vec<String> {
    line.split("<string>")
        .skip(1)
        .filter_map(|x| x.split("</string>").next())
        .map(xml_unescape)
        .collect()
}

fn prefs_file(vm: &mut Vm, file: &str) -> V {
    if let Some(&h) = vm.prefs_files.get(file) {
        return V::Ref(h);
    }
    let mut entries = Vec::new();
    let path = io::fspath(vm, &format!("/data/local/ovelo/shared_prefs/{file}.xml"));
    if let Ok(xml) = std::fs::read_to_string(&path) {
        for line in xml.lines().map(str::trim_start) {
            let Some(name) = pref_name(line) else {
                continue;
            };
            let key = jstr(vm, &name);
            let v = if line.starts_with("<boolean") {
                line.contains("value=\"true\"")
                    .then_some(V::I32(1))
                    .or(Some(V::I32(0)))
            } else if line.starts_with("<int") {
                line.split("value=\"")
                    .nth(1)
                    .and_then(|x| x.split('\"').next())
                    .and_then(|x| x.parse::<i32>().ok())
                    .map(V::I32)
            } else if line.starts_with("<long") {
                line.split("value=\"")
                    .nth(1)
                    .and_then(|x| x.split('\"').next())
                    .and_then(|x| x.parse::<i64>().ok())
                    .map(V::I64)
            } else if line.starts_with("<float") {
                line.split("value=\"")
                    .nth(1)
                    .and_then(|x| x.split('\"').next())
                    .and_then(|x| x.parse::<f32>().ok())
                    .map(V::F32)
            } else if line.starts_with("<string") {
                pref_strings(line).first().map(|x| jstr(vm, x))
            } else if line.starts_with("<set") {
                let items = pref_strings(line)
                    .into_iter()
                    .map(|x| jstr(vm, &x))
                    .collect();
                Some(host(
                    vm,
                    "Ljava/util/LinkedHashSet;",
                    HostState::List(items),
                ))
            } else {
                None
            };
            if let Some(v) = v {
                entries.push((key, v));
            }
        }
    }
    let V::Ref(h) = host(
        vm,
        "Landroid/content/SharedPreferences;",
        HostState::Map(entries),
    ) else {
        unreachable!()
    };
    vm.prefs_files.insert(file.to_string(), h);
    V::Ref(h)
}

fn save_prefs(vm: &mut Vm, file: &str, h: u32) {
    let Some(Obj::Host {
        state: HostState::Map(m),
        ..
    }) = vm.heap.get(h)
    else {
        return;
    };
    let mut out = String::from("<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map>\n");
    for (k, v) in m.clone() {
        let Some(key) = sv(vm, k) else { continue };
        let key = xml_escape(&key);
        match v {
            V::I32(x) => out.push_str(&format!("<int name=\"{key}\" value=\"{x}\" />\n")),
            V::I64(x) => out.push_str(&format!("<long name=\"{key}\" value=\"{x}\" />\n")),
            V::F32(x) => out.push_str(&format!("<float name=\"{key}\" value=\"{x}\" />\n")),
            V::Ref(rh) => {
                if let Some(x) = sv(vm, v) {
                    out.push_str(&format!(
                        "<string name=\"{key}\">{}</string>\n",
                        xml_escape(&x)
                    ));
                } else if let Some(items) = list_snapshot(vm, rh) {
                    let items: Vec<String> = items
                        .iter()
                        .filter_map(|item| sv(vm, *item).map(|x| xml_escape(&x)))
                        .collect();
                    out.push_str(&format!("<set name=\"{key}\">"));
                    for item in items {
                        out.push_str(&format!("<string>{item}</string>"));
                    }
                    out.push_str("</set>\n");
                }
            }
            _ => {}
        }
    }
    out.push_str("</map>\n");
    let path = io::fspath(vm, &format!("/data/local/ovelo/shared_prefs/{file}.xml"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, out);
}

/// The drawable's cached bounds Rect handle (None until created).
fn bounds_h(vm: &Vm, h: u32) -> Option<u32> {
    match pseudo_get(vm, h, P_BOUNDS) {
        Some(V::Ref(rh)) => Some(rh),
        _ => None,
    }
}

/// `getBounds()`: lazily create and cache a `Rect` on the drawable so
/// later `setBounds` writes land in the same object the guest reads.
fn bounds_rect(vm: &mut Vm, h: u32) -> V {
    if let Some(rh) = bounds_h(vm, h) {
        return V::Ref(rh);
    }
    let rh = host(vm, "Landroid/graphics/Rect;", HostState::Empty);
    pseudo_set(vm, h, P_BOUNDS, rh);
    rh
}

// ---------------------------------------------------------------------
// Bitmap decode / scale
//

/// Drain an `InputStream`-ish handle to bytes: host Bytes/Path fast
/// path, then a `read(byte[],off,len)` loop for guest wrappers
/// (Glide's RecyclableBufferedInputStream, ContentLengthInputStream…).
fn stream_bytes(vm: &mut Vm, h: u32) -> Vec<u8> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Bytes(b, pos),
            ..
        }) => return b[*pos..].to_vec(),
        Some(Obj::Host {
            state: HostState::Path(p),
            ..
        }) => return std::fs::read(io::fspath(vm, p)).unwrap_or_default(),
        _ => {}
    }
    const CHUNK: usize = 65536;
    let buf = vm.heap.alloc(Obj::Array {
        elem: "B".into(),
        data: vec![V::I32(0); CHUNK],
    });
    let mut out = Vec::new();
    for _ in 0..4096 {
        let r = vm.call_virtual_sig(
            h,
            "read",
            &["[B", "I", "I"],
            &[V::Ref(buf), V::I32(0), V::I32(CHUNK as i32)],
        );
        let n = match r {
            Ok(Some(V::I32(n))) if n > 0 => n as usize,
            _ => break,
        };
        if let Some(Obj::Array { data, .. }) = vm.heap.get(buf) {
            out.extend(data[..n.min(data.len())].iter().map(|v| i32v(*v) as u8));
        } else {
            break;
        }
    }
    out
}

/// Decode real image bytes into a `Bitmap` host object. Empty or
/// undecodable input -> null, matching `BitmapFactory` returning null.
fn bitmap_decode(vm: &mut Vm, bytes: &[u8]) -> Option<V> {
    // Undecodable input -> null, matching `BitmapFactory` returning null
    // for corrupt/unsupported images (guest then null-checks normally).
    let Ok(img) = image::load_from_memory(bytes) else {
        return Some(V::Ref(0));
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(host(
        vm,
        "Landroid/graphics/Bitmap;",
        HostState::Bitmap {
            w,
            h,
            rgba: std::rc::Rc::new(rgba.into_raw()),
        },
    ))
}

/// Decode honoring `Options.inBitmap`: Glide's BitmapPool pre-allocates
/// the destination Bitmap and reads it back from the options object —
/// ignoring the returned handle would leave the pool bitmap blank.
/// Pixels are scaled into the reused bitmap's dimensions.
fn bitmap_decode_opts(vm: &mut Vm, bytes: &[u8], args: &[V]) -> Option<V> {
    let in_bm = args.iter().find_map(|a| match a {
        V::Ref(h) if *h != 0 => vm
            .get_object_field(*h, "Landroid/graphics/BitmapFactory$Options;", "inBitmap")
            .and_then(|v| match v {
                V::Ref(b) if b != 0 => Some(b),
                _ => None,
            }),
        _ => None,
    });
    if let Some(in_bm) = in_bm {
        if let Ok(img) = image::load_from_memory(bytes) {
            let rgba = img.to_rgba8();
            let (sw, sh) = rgba.dimensions();
            let raw = rgba.into_raw();
            if let Some(Obj::Host {
                state: HostState::Bitmap { w, h, rgba: dst },
                ..
            }) = vm.heap.get_mut(in_bm)
            {
                *dst = std::rc::Rc::new(if (*w, *h) == (sw, sh) {
                    raw
                } else {
                    scale_rgba(sw, sh, &raw, *w, *h)
                });
                return Some(V::Ref(in_bm));
            }
        }
    }
    bitmap_decode(vm, bytes)
}

/// (w, h) of a Bitmap host; (0, 0) for anything else.
pub(crate) fn bitmap_dims(vm: &Vm, v: V) -> (u32, u32) {
    match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::Bitmap { w, h, .. },
                ..
            }) => (*w, *h),
            _ => (0, 0),
        },
        _ => (0, 0),
    }
}

/// (w, h, pixels) of a Bitmap host — or of a `Drawable`/`ImageView`-
/// like object holding a bitmap in any ref field (BitmapDrawable.mBitmapState.mBitmap,
/// Glide's BitmapDrawable, …).
pub(crate) fn bitmap_pixels(vm: &Vm, v: V) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    bitmap_pixels_at(vm, v, 0)
}

fn bitmap_pixels_at(vm: &Vm, v: V, depth: u32) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    // Depth bound: drawable graphs can be cyclic (a container holding a
    // ref back to itself) — without this the field walk overflows the
    // host stack.
    if depth > 8 {
        return None;
    }
    let V::Ref(h) = v else { return None };
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Bitmap { w, h, rgba },
            ..
        }) => Some((*w, *h, rgba.clone())),
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => fields
            .values()
            .find_map(|f| bitmap_pixels_at(vm, *f, depth + 1)),
        // LayerDrawable/TransitionDrawable keep drawables in an array.
        Some(Obj::Array { data, .. }) => data
            .iter()
            .find_map(|f| bitmap_pixels_at(vm, *f, depth + 1)),
        _ => None,
    }
}

/// Nearest-neighbor RGBA8 scale.
fn scale_rgba(sw: u32, sh: u32, src: &[u8], dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    for y in 0..dh {
        for x in 0..dw {
            let sx = (x * sw / dw).min(sw - 1);
            let sy = (y * sh / dh).min(sh - 1);
            let s = ((sy * sw + sx) * 4) as usize;
            let d = ((y * dw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

/// Guest filesystem path behind a `content://media/.../<id>` uri (via the
/// runtime's media id table), a `file://` uri, or a raw absolute path.
pub(crate) fn media_uri_path(vm: &Vm, uri: &str) -> Option<String> {
    if uri.is_empty() {
        return None;
    }
    if let Some(rest) = uri.strip_prefix("file://") {
        return Some(rest.to_string());
    }
    if uri.starts_with('/') {
        return Some(uri.to_string());
    }
    if uri.starts_with("content://media/") {
        if let Some(id) = uri.rsplit('/').next().and_then(|t| t.parse::<i64>().ok()) {
            for (data, i) in &vm.media_ids {
                if *i == id {
                    return Some(data.clone());
                }
            }
        }
    }
    if uri.contains("externalstorage.documents") {
        // Return the GUEST path — callers pass it through `io::fspath`
        // which maps the external-storage alias back to external_root.
        let (_id, rel) = db::saf_doc_rel(uri);
        return Some(format!("/storage/emulated/0/{rel}"));
    }
    None
}
