//! java.io / java.net / java.nio / regex / SimpleDateFormat / org.json.

use super::*;
use crate::value::{HostState, Obj, V};

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(
        if desc_is_a(class, "Ljava/io/PrintStream;") || desc_is_a(class, "Ljava/io/PrintWriter;") {
            print_call(vm, name, sig, args)
        } else if class.ends_with("FileInputStream;") && name == "<init>" {
            // Open the real file: arg is a File (Path host), a String, or a
            // FileDescriptor already carrying bytes (`getFD`/ParcelFileDescriptor).
            let arg = args.get(1).copied().unwrap_or(V::Ref(0));
            let fd_bytes = match arg {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Bytes(b, p),
                        ..
                    }) => Some(b[*p..].to_vec()),
                    _ => None,
                },
                _ => None,
            };
            let bytes = fd_bytes.unwrap_or_else(|| {
                stream_arg_path(vm, arg)
                    .and_then(|p| std::fs::read(fspath(vm, &p)).ok())
                    .unwrap_or_default()
            });
            if let Some(V::Ref(this)) = args.first() {
                let (ci, d, fs) = match vm.heap.get(*this) {
                    Some(Obj::Host {
                        class_idx,
                        desc,
                        fields,
                        ..
                    }) => (*class_idx, desc.clone(), fields.clone()),
                    Some(Obj::Instance { class_idx, fields }) => {
                        (*class_idx, class.to_string(), fields.clone())
                    }
                    _ => (u64::MAX, class.to_string(), Default::default()),
                };
                if let Some(o) = vm.heap.get_mut(*this) {
                    *o = Obj::Host {
                        class_idx: ci,
                        desc: d,
                        fields: fs,
                        state: HostState::Bytes(std::rc::Rc::new(bytes), 0),
                    };
                }
            }
            Some(None)
        } else if class == "Ljava/io/ByteArrayInputStream;" && name == "<init>" {
            // ByteArrayInputStream(byte[]) / (byte[], off, len): store the
            // payload so reads serve the real bytes.
            let arg = args.get(1).copied().unwrap_or(V::Ref(0));
            let mut bytes: Vec<u8> = match arg {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data
                        .iter()
                        .map(|v| i32v(*v) as u8)
                        .collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if args.len() > 3 {
                let off = i64v(args[2]).max(0) as usize;
                let len = i64v(args[3]).max(0) as usize;
                bytes = bytes
                    .into_iter()
                    .skip(off)
                    .take(len)
                    .collect();
            }
            if let Some(V::Ref(this)) = args.first() {
                if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(*this) {
                    *state = HostState::Bytes(std::rc::Rc::new(bytes), 0);
                }
            }
            Some(None)
        } else if desc_is_a(class, "Ljava/io/FilterInputStream;") && name == "<init>" {
            // Guest subclasses read the wrapped stream straight out of the
            // `in` field — the java ctor would have stored it.
            if let Some(V::Ref(this)) = args.first() {
                let wrapped = args.get(1).copied().unwrap_or(V::Ref(0));
                field_set(vm, *this, "Ljava/io/FilterInputStream;", "in", wrapped);
            }
            Some(None)
        } else if class.ends_with("FileInputStream;") && name == "getFD" {
            // The descriptor keeps the byte payload so decodeFileDescriptor
            // can read it without a second open.
            let (bytes, pos) = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
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
                HostState::Bytes(bytes, pos),
            )))
        } else if desc_is_a(class, "Ljava/io/InputStream;")
            || class.ends_with("InputStream;")
            || class.ends_with("Reader;")
            || class.ends_with("OutputStream;")
            || class.ends_with("Writer;")
        {
            stream_call(vm, name, args)?
        } else if class == "Ljava/io/File;" {
            file_call(vm, name, args)
        } else if class == "Ljava/util/regex/Pattern;" {
            pattern_call(vm, name, args)
        } else if class == "Ljava/util/regex/Matcher;" {
            matcher_call(vm, name, args)?
        } else if desc_is_a(class, "Ljava/text/SimpleDateFormat;")
            || desc_is_a(class, "Ljava/text/DateFormat;")
            || class == "Ljava/text/NumberFormat;"
            || class == "Ljava/text/DecimalFormat;"
        {
            format_call(vm, name, args)
        } else if class == "Lorg/json/JSONObject;" {
            json_obj(vm, name, args)
        } else if class == "Lorg/json/JSONArray;" {
            json_arr(vm, name, args)
        } else if class == "Ljavax/net/ssl/TrustManagerFactory;" {
            match name {
                // OkHttp's Platform requires exactly one default trust
                // manager — answer a single X509TrustManager placeholder.
                "getTrustManagers" => {
                    let tm = host(vm, "Ljavax/net/ssl/X509TrustManager;", HostState::Empty);
                    Some(Some(arr(vm, "Ljavax/net/ssl/TrustManager;", vec![tm])))
                }
                "getDefaultAlgorithm" => Some(Some(jstr(vm, "PKIX"))),
                _ => Some(Some(stub_ret(vm, sig))),
            }
        } else if class == "Ljavax/net/ssl/X509TrustManager;"
            || class == "Ljavax/net/ssl/TrustManager;"
            || class == "Ljavax/net/ssl/SSLSocketFactory;"
            || class == "Ljavax/net/ssl/SSLContext;"
        {
            match name {
                "getAcceptedIssuers" => Some(Some(arr(
                    vm,
                    "Ljava/security/cert/X509Certificate;",
                    vec![],
                ))),
                "init" | "checkClientTrusted" | "checkServerTrusted" => Some(None),
                "getSocketFactory" | "getDefault" => Some(Some(host(
                    vm,
                    "Ljavax/net/ssl/SSLSocketFactory;",
                    HostState::Empty,
                ))),
                "getInstance" => Some(Some(host(vm, class, HostState::Empty))),
                _ => Some(Some(stub_ret(vm, sig))),
            }
        } else if class == "Ljava/net/URLEncoder;" || class == "Ljava/net/URLDecoder;" {
            url_codec(vm, name, args)
        } else if class == "Ljava/net/URL;" || class == "Ljava/net/URI;" {
            url_call(vm, name, args)?
        } else if class == "Ljava/net/URLConnection;"
            || desc_is_a(class, "Ljava/net/URLConnection;")
        {
            match name {
                "connect"
                | "disconnect"
                | "setRequestProperty"
                | "setDoInput"
                | "setDoOutput"
                | "setConnectTimeout"
                | "setReadTimeout"
                | "setRequestMethod"
                | "setChunkedStreamingMode"
                | "setFixedLengthStreamingMode"
                | "setInstanceFollowRedirects" => Some(None),
                "getResponseCode" => Some(Some(V::I32(200))),
                "getResponseMessage" | "getContentType" | "getContentEncoding"
                | "getHeaderField" | "getRequestProperty" | "getURL" => Some(Some(V::Ref(0))),
                "getContentLength"
                | "getContentLengthLong"
                | "getDate"
                | "getExpiration"
                | "getLastModified"
                | "getHeaderFieldInt"
                | "getHeaderFieldLong" => Some(Some(V::I64(0))),
                "getInputStream" => Some(Some(host(vm, "Ljava/io/InputStream;", HostState::Empty))),
                "getOutputStream" => {
                    Some(Some(host(vm, "Ljava/io/OutputStream;", HostState::Empty)))
                }
                "getHeaderFields" => {
                    Some(Some(host(vm, "Ljava/util/Map;", HostState::Map(vec![]))))
                }
                _ => None,
            }
        } else if desc_is_a(class, "Ljava/nio/charset/Charset;")
            || class == "Ljava/nio/charset/StandardCharsets;"
        {
            charset_call(vm, name, args)
        } else if desc_is_a(class, "Ljava/nio/Buffer;") || class.starts_with("Ljava/nio/") {
            nio_call(vm, name, args)
        } else if class == "Ljava/util/Base64;"
            || class == "Ljava/util/Base64$Encoder;"
            || class == "Ljava/util/Base64$Decoder;"
            || class == "Landroid/util/Base64;"
        {
            base64_call(vm, name, args)
        } else if class == "Ljava/util/Formatter;"
            || class == "Ljava/util/Scanner;"
            || class == "Ljava/util/Locale;"
            || class == "Ljava/text/MessageFormat;"
            || class.starts_with("Ljava/text/")
        {
            misc_fmt(vm, class, name, args)
        } else {
            return Ok(None);
        },
    )
}

// ---------------------------------------------------------------------
// PrintStream / PrintWriter — mirror to stderr

fn print_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "print" | "println" => {
            let t = if args.len() > 1 {
                v_to_s(vm, args[1])
            } else {
                String::new()
            };
            if name == "println" {
                eprintln!("{t}");
            } else {
                eprint!("{t}");
            }
            Some(None)
        }
        "printf" | "format" => {
            let fmt = if args.len() > 2 {
                sv(vm, args[2]).unwrap_or_default()
            } else {
                sv(vm, args[1]).unwrap_or_default()
            };
            let flat: Vec<V> = args
                .iter()
                .skip(if args.len() > 2 { 3 } else { 2 })
                .copied()
                .collect();
            eprint!("{}", simple_format(vm, &fmt, &flat));
            Some(Some(args[0]))
        }
        "write" | "flush" | "close" | "checkError" | "append" => {
            if name == "checkError" {
                Some(Some(V::I32(0)))
            } else if name == "append" {
                Some(Some(args[0]))
            } else {
                Some(None)
            }
        }
        _ => None,
    }
}

fn stream_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    // Byte-backed streams (raw resources, assets): real reads.
    if let Some(Obj::Host {
        state: HostState::Bytes(data, pos),
        ..
    }) = vm.heap.get_mut(this)
    {
        match name {
            "read" => {
                if args.len() == 1 {
                    return Ok(Some(Some(if *pos < data.len() {
                        let b = data[*pos] as i32;
                        *pos += 1;
                        V::I32(b)
                    } else {
                        V::I32(-1)
                    })));
                }
                // read(byte[]) / read(byte[], off, len)
                let buf = match args.get(1) {
                    Some(&V::Ref(h)) => h,
                    _ => 0,
                };
                let (mut off, mut want) = (0usize, usize::MAX);
                if args.len() > 3 {
                    off = i64v(args[2]).max(0) as usize;
                    want = i64v(args[3]).max(0) as usize;
                }
                let avail = data.len() - *pos;
                if avail == 0 && want != 0 {
                    return Ok(Some(Some(V::I32(-1))));
                }
                let n = want.min(avail);
                let chunk: Vec<u8> = data[*pos..*pos + n].to_vec();
                *pos += n;
                if let Some(Obj::Array { data: dst, .. }) = vm.heap.get_mut(buf) {
                    for (i, b) in chunk.iter().enumerate() {
                        if let Some(slot) = dst.get_mut(off + i) {
                            *slot = V::I32(*b as i8 as i32);
                        }
                    }
                }
                return Ok(Some(Some(V::I32(n as i32))));
            }
            "available" => {
                return Ok(Some(Some(V::I32((data.len() - *pos) as i32))));
            }
            "skip" => {
                let n = i64v(args.get(1).copied().unwrap_or(V::I64(0))).max(0) as usize;
                let took = n.min(data.len() - *pos);
                *pos += took;
                return Ok(Some(Some(V::I64(took as i64))));
            }
            "reset" => {
                *pos = 0;
                return Ok(Some(None));
            }
            "mark" | "close" => return Ok(Some(None)),
            "markSupported" => return Ok(Some(Some(V::I32(1)))),
            _ => {}
        }
    }
    // `FilterInputStream`-style wrappers: the java methods we don't
    // model delegate to the protected `in` field (stored by the
    // `<init>` shim). A guest IOException propagates so the app's own
    // catch blocks see it.
    if matches!(
        name,
        "read" | "available" | "skip" | "mark" | "reset" | "markSupported" | "ready" | "close"
    ) {
        if let Some(V::Ref(inner)) =
            field_get(vm, this, "Ljava/io/FilterInputStream;", "in")
        {
            if inner != 0 && inner != this {
                if std::env::var_os("OVELO_STREAMDBG").is_some() {
                    let td = match vm.heap.get(this) {
                        Some(Obj::Host { desc, .. }) => desc.clone(),
                        Some(Obj::Instance { class_idx, .. }) => {
                            vm.type_desc_of(*class_idx).unwrap_or_default()
                        }
                        _ => "?".into(),
                    };
                    let id = match vm.heap.get(inner) {
                        Some(Obj::Host { desc, .. }) => desc.clone(),
                        Some(Obj::Instance { class_idx, .. }) => {
                            vm.type_desc_of(*class_idx).unwrap_or_default()
                        }
                        _ => "?".into(),
                    };
                    eprintln!("[sdbg] {name} {td}#{this} -> {id}#{inner}");
                }
                if let Some(v) = vm.call_virtual_rt(inner, name, &args[1..])? {
                    if matches!(name, "mark" | "reset" | "close") {
                        return Ok(Some(None));
                    }
                    return Ok(Some(Some(v)));
                }
                // Host receivers (ByteArrayInputStream, another builtin
                // wrapper, ...) have no dex method to resolve — re-enter
                // this dispatcher with `inner` as `this` so byte-backed
                // reads and deeper `in` chains keep working.
                let mut inner_args = Vec::with_capacity(args.len());
                inner_args.push(V::Ref(inner));
                inner_args.extend_from_slice(&args[1..]);
                if let Some(res) = stream_call(vm, name, &inner_args)? {
                    return Ok(Some(res));
                }
            }
        }
    }
    let _ = vm;
    // Real writable files (SAF `openOutputStream` → FileOutputStream):
    // `write`/`flush`/`close` hit the host file so guest writes persist.
    if matches!(name, "write" | "flush" | "close")
        && matches!(
            vm.heap.get(this),
            Some(Obj::Host {
                state: HostState::File(_),
                ..
            })
        )
    {
        if name == "write" {
            // write(byte[]) has the same arity as write(int) — the
            // argument's TYPE decides: a Ref points at a byte array.
            let is_arr = matches!(
                vm.heap.get(match args.get(1) {
                    Some(&V::Ref(h)) => h,
                    _ => 0,
                }),
                Some(Obj::Array { .. })
            );
            let bytes: Vec<u8> = if args.len() == 2 && !is_arr {
                vec![i64v(args[1]) as u8]
            } else {
                let arr_h = match args.get(1) {
                    Some(&V::Ref(h)) => h,
                    _ => 0,
                };
                let (off, len) = if args.len() > 3 {
                    (
                        i64v(args[2]).max(0) as usize,
                        i64v(args[3]).max(0) as usize,
                    )
                } else {
                    (0, usize::MAX)
                };
                match vm.heap.get(arr_h) {
                    Some(Obj::Array { data, .. }) => data
                        .iter()
                        .skip(off)
                        .take(len)
                        .map(|v| i64v(*v) as u8)
                        .collect(),
                    _ => Vec::new(),
                }
            };
            if let Some(Obj::Host {
                state: HostState::File(f),
                ..
            }) = vm.heap.get_mut(this)
            {
                use std::io::Write as _;
                let _ = f.write_all(&bytes);
            }
        } else if let Some(Obj::Host {
            state: HostState::File(f),
            ..
        }) = vm.heap.get_mut(this)
        {
            use std::io::Write as _;
            let _ = f.flush();
        }
        return Ok(Some(None));
    }
    Ok(match name {
        "read" => {
            // Streams without a byte payload report EOF so callers exit
            // loops cleanly.
            Some(Some(V::I32(-1)))
        }
        "available" => Some(Some(V::I32(0))),
        "readLine" => Some(Some(V::Ref(0))),
        "ready" | "markSupported" => Some(Some(V::I32(0))),
        "skip" => Some(Some(V::I64(0))),
        "write" | "flush" | "close" | "mark" | "reset" => Some(None),
        "toByteArray" => {
            let _ = args;
            Some(Some(V::Ref(0)))
        }
        "toString" if args.len() == 1 => Some(Some(V::Ref(0))),
        "size" => Some(Some(V::I32(0))),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// File

/// Guest-visible path -> host path under `vm.guest_root`. Absolute
/// guest paths and `..` can never escape the sandbox: root/prefix
/// components are dropped and `..` above the root is discarded.
pub(crate) fn fspath(vm: &Vm, guest: &str) -> std::path::PathBuf {
    // External-storage aliases resolve against external_root — shared
    // media the app-private guest_root has no business holding.
    for pre in [
        "/storage/emulated/0",
        "/storage/self/primary",
        "/mnt/sdcard",
        "/sdcard",
    ] {
        if guest == pre {
            return vm.external_root.clone();
        }
        if let Some(rest) = guest.strip_prefix(&format!("{pre}/")) {
            return under(&vm.external_root, rest);
        }
    }
    under(&vm.guest_root, guest)
}

/// Sanitized join: `..` can't climb above `root`, absolute/CurDir parts
/// are ignored. Used for both guest_root and external_root mappings.
fn under(root: &std::path::Path, guest: &str) -> std::path::PathBuf {
    use std::path::Component as C;
    let mut out = root.to_path_buf();
    let mut depth = 0usize;
    for c in std::path::Path::new(guest).components() {
        match c {
            C::RootDir | C::Prefix(_) | C::CurDir => {}
            C::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                }
            }
            C::Normal(p) => {
                out.push(p);
                depth += 1;
            }
        }
    }
    out
}

fn file_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let path = s(
        vm,
        match args[0] {
            V::Ref(h) => h,
            _ => 0,
        },
    )
    .unwrap_or_default();
    let std_path = fspath(vm, &path);
    match name {
        "<init>" => {
            // File(path) / File(parent, child) — fold the args into the
            // receiver's stored path. A File parent contributes its own
            // stored path; everything else reads as its string form.
            let parts: Vec<String> = args[1..]
                .iter()
                .map(|v| sv(vm, *v).unwrap_or_default())
                .filter(|p| !p.is_empty())
                .collect();
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(*h) {
                    *state = HostState::Path(parts.join("/"));
                }
            }
            // Void: the class-not-in-DEX stub already answers handled.
            None
        }
        "getPath" | "toString" => Some(jstr(vm, &path)),
        "getAbsolutePath" | "getCanonicalPath" => {
            let p = if path.starts_with('/') {
                path.clone()
            } else {
                format!("/data/local/ovelo/{path}")
            };
            Some(jstr(vm, &p))
        }
        "getName" => {
            let n = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            Some(jstr(vm, &n))
        }
        "getParent" | "getParentFile" => {
            let p = std::path::Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if name == "getParent" {
                Some(jstr(vm, &p))
            } else {
                Some(host(vm, "Ljava/io/File;", HostState::Path(p)))
            }
        }
        "exists" => Some(V::I32(std_path.exists() as i32)),
        "isDirectory" => Some(V::I32(std_path.is_dir() as i32)),
        "isFile" => Some(V::I32(std_path.is_file() as i32)),
        "isAbsolute" => Some(V::I32(path.starts_with('/') as i32)),
        "canRead" | "canWrite" | "canExecute" => Some(V::I32(1)),
        "length" => Some(V::I64(
            std_path.metadata().map(|m| m.len() as i64).unwrap_or(0),
        )),
        "lastModified" => Some(V::I64(
            std_path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        )),
        "mkdir" | "mkdirs" => Some(V::I32(std::fs::create_dir_all(&std_path).is_ok() as i32)),
        "delete" => {
            // `new File("")` maps onto the storage root — deleting it
            // would wipe the whole sandbox, which a failed delete must
            // never do.
            let guarded = std_path == vm.guest_root || std_path == vm.external_root;
            Some(V::I32(
                (!guarded
                    && (std::fs::remove_file(&std_path)
                        .or_else(|_| std::fs::remove_dir_all(&std_path))
                        .is_ok())) as i32,
            ))
        }
        "createNewFile" => Some(V::I32(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&std_path)
                .is_ok() as i32,
        )),
        "renameTo" => {
            let to = s(
                vm,
                match args[1] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(V::I32(
                std::fs::rename(&std_path, fspath(vm, &to)).is_ok() as i32
            ))
        }
        "list" | "listFiles" => {
            let names: Vec<String> = std::fs::read_dir(&std_path)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            if std::env::var_os("OVELO_VIEWDBG").is_some() {
                eprintln!(
                    "[viewdbg] {name} {path} -> {} ({} entries)",
                    std_path.display(),
                    names.len()
                );
            }
            if name == "list" {
                let vs: Vec<V> = names.iter().map(|n| jstr(vm, n)).collect();
                Some(arr(vm, STRING, vs))
            } else {
                let mut out = vec![];
                for n in names {
                    let full = format!("{path}/{n}");
                    out.push(host(vm, "Ljava/io/File;", HostState::Path(full)));
                }
                Some(arr(vm, "Ljava/io/File;", out))
            }
        }
        "getFreeSpace" | "getTotalSpace" | "getUsableSpace" => Some(V::I64(1i64 << 32)),
        // Hidden on Android == getName() starts with '.' — Java's
        // getName is the raw last segment, so "." and ".." count too
        // (std::path's file_name would normalize them away).
        "isHidden" => Some(V::I32(
            path.trim_end_matches('/')
                .rsplit('/')
                .next()
                .is_some_and(|n| n.starts_with('.')) as i32,
        )),
        "setReadable" | "setWritable" | "setExecutable" | "setReadOnly" | "setLastModified"
        | "deleteOnExit" | "toPath" | "toURI" | "toURL" | "listRoots" | "createTempFile"
        | "compareTo" => {
            if name == "listRoots" || name == "createTempFile" {
                Some(host(vm, "Ljava/io/File;", HostState::Path("/tmp/x".into())))
            } else if matches!(name, "toPath" | "toURI" | "toURL") {
                Some(V::Ref(0))
            } else if name == "compareTo" {
                Some(V::I32(0))
            } else {
                Some(V::I32(1))
            }
        }
        "equals" | "hashCode" => Some(V::I32(1)),
        _ => return None,
    }
    .map(Some)
}

// ---------------------------------------------------------------------
// regex (literal / light-meta matcher)

/// Test `text` against a regex-ish pattern: handles literal text plus
/// `.*`, `.`, character classes, and `?`/`+` quantified single chars —
/// anything more exotic falls back to literal equality.
pub(crate) fn literal_match(pat: &str, text: &str) -> bool {
    if pat == text {
        return true;
    }
    // Very small recursive matcher for common metachars.
    fn class_ok(spec: &[char], c: char) -> bool {
        let (neg, spec) = match spec.first() {
            Some('^') => (true, &spec[1..]),
            _ => (false, spec),
        };
        let mut hit = false;
        let mut i = 0;
        while i < spec.len() {
            let lo = if spec[i] == '\\' && i + 1 < spec.len() {
                i += 1;
                match spec[i] {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'd' | 'w' | 's' => {
                        // \d \w \s inside a class — set membership, no range
                        let ok = match spec[i] {
                            'd' => c.is_ascii_digit(),
                            'w' => c.is_alphanumeric() || c == '_',
                            _ => c.is_whitespace(),
                        };
                        hit |= ok;
                        i += 1;
                        continue;
                    }
                    x => x,
                }
            } else {
                spec[i]
            };
            if i + 2 < spec.len() && spec[i + 1] == '-' {
                hit |= (lo..=spec[i + 2]).contains(&c);
                i += 3;
            } else {
                hit |= lo == c;
                i += 1;
            }
        }
        hit != neg
    }
    // Position of the `]` that closes a class starting at p[0]='['.
    fn class_end(p: &[char]) -> Option<usize> {
        let mut i = 1;
        while i < p.len() {
            match p[i] {
                '\\' => i += 1,
                ']' if i > 1 => return Some(i),
                _ => {}
            }
            i += 1;
        }
        None
    }
    fn m(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        // [...] with an optional *, + or ? quantifier.
        if p[0] == '[' {
            if let Some(end) = class_end(p) {
                let spec = &p[1..end];
                let after = &p[end + 1..];
                let (quant, rest): (char, &[char]) = match after.first() {
                    Some('*') => ('*', &after[1..]),
                    Some('+') => ('+', &after[1..]),
                    Some('?') => ('?', &after[1..]),
                    _ => ('\0', after),
                };
                return match quant {
                    '*' | '+' => {
                        let mut max = 0;
                        while max < t.len() && class_ok(spec, t[max]) {
                            max += 1;
                        }
                        // `+` requires at least one consumed char;
                        // `*` may consume none.
                        let lo = usize::from(quant == '+');
                        for skip in (lo..=max).rev() {
                            if m(rest, &t[skip..]) {
                                return true;
                            }
                        }
                        false
                    }
                    '?' => {
                        (!t.is_empty() && class_ok(spec, t[0]) && m(rest, &t[1..]))
                            || m(rest, t)
                    }
                    _ => !t.is_empty() && class_ok(spec, t[0]) && m(rest, &t[1..]),
                };
            }
        }
        match p[0] {
            '*' => false, // stray star
            '.' if p.len() > 1 && p[1] == '*' => {
                // .* — try consuming 0..n chars
                let rest = &p[2..];
                if rest.is_empty() {
                    return true;
                }
                for skip in 0..=t.len() {
                    if m(rest, &t[skip..]) {
                        return true;
                    }
                }
                false
            }
            c if p.len() > 1 && p[1] == '*' => {
                // x*
                if !t.is_empty() && (t[0] == c || c == '.') && m(p, &t[1..]) {
                    return true;
                }
                m(&p[2..], t)
            }
            c if p.len() > 1 && p[1] == '+' => {
                if t.is_empty() || (t[0] != c && c != '.') {
                    return false;
                }
                for skip in 1..=t.len() {
                    if m(&p[2..], &t[skip..]) {
                        return true;
                    }
                }
                false
            }
            '\\' if p.len() > 1 => {
                // \d \w \s
                let cls = p[1];
                if t.is_empty() {
                    return false;
                }
                let ok = match cls {
                    'd' => t[0].is_ascii_digit(),
                    'w' => t[0].is_alphanumeric() || t[0] == '_',
                    's' => t[0].is_whitespace(),
                    c => t[0] == c,
                };
                ok && m(&p[2..], &t[1..])
            }
            '^' => m(&p[1..], t),
            '$' if p.len() == 1 => t.is_empty(),
            c => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    m(&p, &t)
}

fn pattern_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "compile" => {
            let pat = sv(vm, args[0]).unwrap_or_default();
            Some(Some(host(
                vm,
                "Ljava/util/regex/Pattern;",
                HostState::Regex(pat),
            )))
        }
        "matches" => {
            if args.len() == 1 {
                // instance: matcher already bound
                return Some(Some(V::I32(0)));
            }
            let pat = sv(vm, args[0]).unwrap_or_default();
            let t = sv(vm, args[1]).unwrap_or_default();
            Some(Some(V::I32(literal_match(&pat, &t) as i32)))
        }
        "matcher" => {
            let h = vm.alloc_host("Ljava/util/regex/Matcher;", HostState::Slot(args[1]));
            if let V::Ref(ph) = args[0] {
                if let Some(pat) = s(vm, ph) {
                    let pv = jstr(vm, &pat);
                    pseudo_set(vm, h, P_ACTION, pv);
                }
            }
            Some(Some(V::Ref(h)))
        }
        "pattern" | "toString" => {
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
        "split" => {
            let pat = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            let t = sv(vm, args[1]).unwrap_or_default();
            let parts: Vec<V> = t.split(&pat).map(|x| jstr(vm, x)).collect();
            Some(Some(arr(vm, STRING, parts)))
        }
        "quote" => Some(args.first().copied()),
        _ => None,
    }
}

fn matcher_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let text = slot_of(vm, h).and_then(|v| sv(vm, v)).unwrap_or_default();
    let pat = pseudo_get(vm, h, P_ACTION)
        .and_then(|v| sv(vm, v))
        .unwrap_or_default();
    Ok(match name {
        "matches" => Some(Some(V::I32(literal_match(&pat, &text) as i32))),
        "lookingAt" | "find" => {
            let found = literal_match(&pat, &text) || (!pat.is_empty() && text.contains(&pat));
            Some(Some(V::I32(found as i32)))
        }
        "group" => {
            let i = i32v(*args.get(1).unwrap_or(&V::I32(0)));
            let g = if i == 0 { text.clone() } else { String::new() };
            Some(Some(jstr(vm, &g)))
        }
        "groupCount" => Some(Some(V::I32(0))),
        "replaceAll" | "replaceFirst" => {
            let rep = sv(vm, args[1]).unwrap_or_default();
            let out = if pat.is_empty() {
                text.clone()
            } else if name == "replaceFirst" {
                text.replacen(&pat, &rep, 1)
            } else {
                text.replace(&pat, &rep)
            };
            Some(Some(jstr(vm, &out)))
        }
        "quoteReplacement" => Some(args.first().copied()),
        "start" | "end" => Some(Some(V::I32(0))),
        "reset" => Some(Some(args[0])),
        "region"
        | "useAnchoringBounds"
        | "useTransparentBounds"
        | "usePattern"
        | "appendReplacement" => Some(Some(args[0])),
        "appendTail" => Some(Some(args[1])),
        "pattern" => Some(Some(args[0])),
        "requireEnd" | "hasAnchoringBounds" | "hasTransparentBounds" => Some(Some(V::I32(0))),
        "namedGroups" => Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![])))),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// printf-style % formatting

pub(crate) fn simple_format(vm: &mut Vm, fmt: &str, args: &[V]) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut i = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        if chars.peek() == Some(&'n') {
            chars.next();
            out.push('\n');
            continue;
        }
        // flags
        let mut spec = String::new();
        while let Some(&c2) = chars.peek() {
            if c2.is_ascii_alphabetic() {
                chars.next();
                break;
            }
            spec.push(chars.next().unwrap_or_default());
        }
        let conv = chars.next().unwrap_or('s');
        let v = args.get(i).copied().unwrap_or(V::Ref(0));
        i += 1;
        let body = match conv {
            'd' | 'x' | 'X' | 'o' => {
                let n = i64v(match v {
                    V::Ref(h) => slot_of(vm, h).unwrap_or(v),
                    _ => v,
                });
                match conv {
                    'x' => format!("{n:x}"),
                    'X' => format!("{n:X}"),
                    'o' => format!("{n:o}"),
                    _ => format!("{n}"),
                }
            }
            'f' | 'e' | 'g' | 'a' => {
                let f = f64v(match v {
                    V::Ref(h) => slot_of(vm, h).unwrap_or(v),
                    _ => v,
                });
                // honor ".N" precision
                let prec = spec
                    .rsplit('.')
                    .next()
                    .and_then(|s| s.parse::<usize>().ok());
                match (conv, prec) {
                    ('f', Some(p)) => format!("{f:.p$}"),
                    ('f', None) => format!("{f:.6}"),
                    ('e', _) => format!("{f:e}"),
                    _ => fmt_f64(f),
                }
            }
            'c' => char::from_u32(i32v(v) as u32)
                .map(|c| c.to_string())
                .unwrap_or_default(),
            'b' | 'B' => (!is_null(v) && i32v(v) != 0).to_string(),
            's' | 'S' | 'h' | 'H' => {
                let t = v_to_s(vm, v);
                if conv.is_uppercase() {
                    t.to_uppercase()
                } else {
                    t
                }
            }
            't' | 'T' => {
                chars.next(); // eat date suffix char
                "0".to_string()
            }
            _ => v_to_s(vm, v),
        };
        out.push_str(&body);
    }
    out
}

// ---------------------------------------------------------------------
// java.text.Format family

fn format_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "format" => {
            // SimpleDateFormat.format(Date) — produce ISO-ish text from millis
            if let Some(ms) = args.get(1).and_then(|v| match v {
                V::Ref(h) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Millis(m),
                        ..
                    }) => Some(*m),
                    _ => None,
                },
                _ => None,
            }) {
                Some(Some(jstr(vm, &crate::jdk::time::date_to_string(ms))))
            } else {
                let t = v_to_s(vm, *args.get(1).unwrap_or(&V::Ref(0)));
                Some(Some(jstr(vm, &t)))
            }
        }
        "parse" => {
            let t = sv(vm, *args.get(1).unwrap_or(&V::Ref(0))).unwrap_or_default();
            Some(Some(host(
                vm,
                "Ljava/util/Date;",
                HostState::Millis(crate::jdk::time::parse_date(&t)),
            )))
        }
        "applyPattern"
        | "applyLocalizedPattern"
        | "setLenient"
        | "setTimeZone"
        | "setCalendar"
        | "setNumberFormat"
        | "setDateFormatSymbols" => Some(None),
        "toPattern" | "toLocalizedPattern" => Some(Some(jstr(vm, ""))),
        "getTimeZone" => Some(Some(host(
            vm,
            "Ljava/util/TimeZone;",
            HostState::StrBuf("UTC".into()),
        ))),
        "getCalendar" | "clone" => Some(Some(args[0])),
        "isLenient" => Some(Some(V::I32(1))),
        "getDateInstance"
        | "getTimeInstance"
        | "getDateTimeInstance"
        | "getInstance"
        | "getNumberInstance"
        | "getIntegerInstance"
        | "getCurrencyInstance"
        | "getPercentInstance" => Some(Some(host(
            vm,
            "Ljava/text/SimpleDateFormat;",
            HostState::Empty,
        ))),
        "getAvailableLocales" => Some(Some(arr(vm, "Ljava/util/Locale;", vec![]))),
        "get2DigitYearStart" | "set2DigitYearStart" => Some(Some(V::Ref(0))),
        "parseObject" => Some(Some(V::Ref(0))),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// org.json — JSONObject on a Map host, JSONArray on a List host, with a
// small recursive-descent parser for `new JSONObject(String)` / toString.

fn json_ws(b: &[char], mut i: usize) -> usize {
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    i
}

fn json_string(b: &[char], mut i: usize) -> Option<(String, usize)> {
    let mut s = String::new();
    while i < b.len() && b[i] != '"' {
        if b[i] == '\\' && i + 1 < b.len() {
            s.push(match b[i + 1] {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                c => c,
            });
            i += 2;
        } else {
            s.push(b[i]);
            i += 1;
        }
    }
    Some((s, i + 1))
}

fn json_value(vm: &mut Vm, b: &[char], i: &mut usize, depth: u32) -> Option<V> {
    // Host-side recursion on untrusted text: cap nesting so a deep
    // `[[[[...` can't exhaust the native stack past the guest budget.
    if depth > 128 {
        return None;
    }
    *i = json_ws(b, *i);
    match *b.get(*i)? {
        '{' => {
            *i += 1;
            let hv = host(vm, "Lorg/json/JSONObject;", HostState::Map(vec![]));
            let V::Ref(h) = hv else { return None };
            loop {
                *i = json_ws(b, *i);
                match *b.get(*i)? {
                    '}' => {
                        *i += 1;
                        return Some(hv);
                    }
                    '"' => {
                        let (k, n) = json_string(b, *i + 1)?;
                        *i = json_ws(b, n);
                        if b.get(*i) != Some(&':') {
                            return None;
                        }
                        *i += 1;
                        let v = json_value(vm, b, i, depth + 1)?;
                        let kv = jstr(vm, &k);
                        map_put(vm, h, kv, v);
                        *i = json_ws(b, *i);
                        if b.get(*i) == Some(&',') {
                            *i += 1;
                        }
                    }
                    _ => return None,
                }
            }
        }
        '[' => {
            *i += 1;
            let hv = host(vm, "Lorg/json/JSONArray;", HostState::List(vec![]));
            let V::Ref(h) = hv else { return None };
            loop {
                *i = json_ws(b, *i);
                if b.get(*i) == Some(&']') {
                    *i += 1;
                    return Some(hv);
                }
                let v = json_value(vm, b, i, depth + 1)?;
                if let Some(l) = list_mut(vm, h) {
                    l.push(v);
                }
                *i = json_ws(b, *i);
                match b.get(*i)? {
                    ',' => *i += 1,
                    ']' => {
                        *i += 1;
                        return Some(hv);
                    }
                    _ => return None,
                }
            }
        }
        '"' => json_string(b, *i + 1).map(|(s, n)| {
            *i = n;
            jstr(vm, &s)
        }),
        c => {
            // number, true, false, null
            let start = *i;
            let mut end = *i;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || ".-+eE".contains(b[end])) {
                end += 1;
            }
            let tok: String = b[start..end].iter().collect();
            *i = end;
            match tok.as_str() {
                "true" => Some(V::I32(1)),
                "false" => Some(V::I32(0)),
                "null" | "null." | "" => Some(V::Ref(0)),
                _ => {
                    if c == '-' || c.is_ascii_digit() {
                        tok.parse::<i64>()
                            .map(V::I64)
                            .or_else(|_| tok.parse::<f64>().map(V::F64))
                            .ok()
                    } else {
                        None
                    }
                }
            }
        }
    }
}

fn json_dump_v(vm: &Vm, v: V) -> String {
    match v {
        V::Ref(0) => "null".into(),
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Str(s)) => format!("\"{s}\""),
            Some(Obj::Host {
                state: HostState::Map(m),
                ..
            }) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, v)| {
                        let ks = sv(vm, *k).unwrap_or_default();
                        format!("\"{ks}\":{}", json_dump_v(vm, *v))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Some(Obj::Host {
                state: HostState::List(l),
                ..
            }) => format!(
                "[{}]",
                l.iter()
                    .map(|v| json_dump_v(vm, *v))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            _ => "null".into(),
        },
        x => fmt_v(x),
    }
}

fn json_obj(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let h = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => return None,
    };
    match name {
        "<init>" => {
            if let Some(src) = args.get(1).and_then(|v| sv(vm, *v)) {
                let b: Vec<char> = src.chars().collect();
                let mut i = 0;
                if let Some(V::Ref(parsed)) = json_value(vm, &b, &mut i, 0) {
                    if let (Some(Obj::Host {
                        state: HostState::Map(src_m),
                        ..
                    }),) = (vm.heap.get(parsed),)
                    {
                        let pairs = src_m.clone();
                        if let Some(m) = map_mut(vm, h) {
                            *m = pairs;
                        }
                    }
                }
            }
            None
        }
        "put" | "accumulate" | "append" => {
            map_put(vm, h, args[1], args.get(2).copied().unwrap_or(V::Ref(0)));
            Some(V::Ref(h))
        }
        "get" | "opt" | "getJSONObject" | "getJSONArray" => {
            map_get(vm, h, args[1]).or(Some(V::Ref(0)))
        }
        "getString" | "optString" | "getName" => {
            let v = map_get(vm, h, args[1]);
            Some(match v {
                Some(x) => {
                    let t = v_to_s(vm, x);
                    jstr(vm, &t)
                }
                None => jstr(
                    vm,
                    &args.get(2).and_then(|d| sv(vm, *d)).unwrap_or_default(),
                ),
            })
        }
        "getInt" | "optInt" | "getLong" | "optLong" | "getDouble" | "optDouble" | "getBoolean"
        | "optBoolean" => {
            let dflt = args.get(1 + (name.starts_with('o') as usize));
            match map_get(vm, h, args[1]) {
                Some(v) if !matches!(v, V::Ref(0)) => Some(v),
                _ => Some(dflt.copied().unwrap_or(V::I32(0))),
            }
        }
        "has" | "isNull" => {
            let has = map_get(vm, h, args[1]);
            Some(V::I32(
                (if name == "has" {
                    has.is_some()
                } else {
                    has.map(|v| matches!(v, V::Ref(0))).unwrap_or(true)
                }) as i32,
            ))
        }
        "remove" => Some(map_remove(vm, h, args[1]).unwrap_or(V::Ref(0))),
        "length" => Some(V::I32(map_len(vm, h) as i32)),
        "keys" => {
            let ks: Vec<V> = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.iter().map(|(k, _)| *k).collect(),
                _ => vec![],
            };
            Some(V::Ref(vm.alloc_host(
                "Ljava/util/Iterator;",
                HostState::Iter(std::rc::Rc::new(ks), 0),
            )))
        }
        "names" => {
            let ks: Vec<V> = match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.iter().map(|(k, _)| *k).collect(),
                _ => vec![],
            };
            Some(V::Ref(
                vm.alloc_host("Lorg/json/JSONArray;", HostState::List(ks)),
            ))
        }
        "toString" | "toStringInt" => Some(jstr(vm, &json_dump_v(vm, V::Ref(h)))),
        _ => return None,
    }
    .map(Some)
}

fn json_arr(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let h = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => return None,
    };
    match name {
        "<init>" => {
            // JSONArray(String) parses the source; JSONArray(Collection)
            // copies elements. Both land in the object's List state.
            match args.get(1).copied() {
                Some(v) if !matches!(v, V::Ref(0)) => {
                    if let Some(src) = sv(vm, v) {
                        let b: Vec<char> = src.chars().collect();
                        let mut i = 0;
                        if let Some(V::Ref(parsed)) = json_value(vm, &b, &mut i, 0) {
                            let elems = match vm.heap.get(parsed) {
                                Some(Obj::Host {
                                    state: HostState::List(l),
                                    ..
                                }) => l.clone(),
                                _ => vec![],
                            };
                            if let Some(l) = list_mut(vm, h) {
                                *l = elems;
                            }
                        }
                    } else {
                        let rh = match v {
                            V::Ref(r) => r,
                            _ => 0,
                        };
                        let elems = match vm.heap.get(rh) {
                            Some(Obj::Host {
                                state: HostState::List(l),
                                ..
                            }) => l.clone(),
                            _ => vec![],
                        };
                        if let Some(l) = list_mut(vm, h) {
                            *l = elems;
                        }
                    }
                }
                _ => {}
            }
            None
        }
        "put" => {
            if let Some(l) = list_mut(vm, h) {
                l.push(args.get(1).copied().unwrap_or(V::Ref(0)));
            }
            Some(V::Ref(h))
        }
        "get" | "opt" | "getJSONObject" | "getJSONArray" => {
            let i = i32v(args.get(1).copied().unwrap_or(V::I32(0))) as usize;
            Some(
                list_ref(vm, h)
                    .and_then(|l| l.get(i).copied())
                    .unwrap_or(V::Ref(0)),
            )
        }
        "getString" | "optString" => {
            let i = i32v(args[1]) as usize;
            let v = list_ref(vm, h).and_then(|l| l.get(i).copied());
            Some(match v {
                Some(x) => {
                    let t = v_to_s(vm, x);
                    jstr(vm, &t)
                }
                None => jstr(
                    vm,
                    &args.get(2).and_then(|d| sv(vm, *d)).unwrap_or_default(),
                ),
            })
        }
        "getInt" | "optInt" | "getLong" | "optLong" | "getDouble" | "optDouble" | "getBoolean"
        | "optBoolean" => {
            let i = i32v(args[1]) as usize;
            Some(
                list_ref(vm, h)
                    .and_then(|l| l.get(i).copied())
                    .unwrap_or_else(|| args.get(2).copied().unwrap_or(V::I32(0))),
            )
        }
        "length" => Some(V::I32(list_ref(vm, h).map(|l| l.len() as i32).unwrap_or(0))),
        "remove" => {
            let i = i32v(args[1]) as usize;
            let v = list_mut(vm, h)
                .and_then(|l| (i < l.len()).then(|| l.remove(i)))
                .unwrap_or(V::Ref(0));
            Some(v)
        }
        "toString" => Some(jstr(vm, &json_dump_v(vm, V::Ref(h)))),
        _ => return None,
    }
    .map(Some)
}

// ---------------------------------------------------------------------
// URL codec / URL / Charset / nio / Base64 / misc

fn url_codec(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let t = sv(vm, args[0]).unwrap_or_default();
    let out = if name == "encode" {
        t.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.*".contains(c) {
                    c.to_string()
                } else if c == ' ' {
                    "+".into()
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
            } else if c == '+' {
                out.push(' ');
            } else {
                out.push(c);
            }
        }
        out
    };
    Some(Some(jstr(vm, &out)))
}

fn url_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "openConnection" | "openStream" | "getContent" | "toURI" | "toURL" => {
            Some(Some(host(vm, "Ljava/net/URLConnection;", HostState::Empty)))
        }
        "getProtocol" | "getHost" | "getPath" | "getFile" | "getQuery" | "getAuthority"
        | "getUserInfo" | "getRef" | "toExternalForm" | "toString" => {
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
        "getPort" | "getDefaultPort" => Some(Some(V::I32(-1))),
        "sameFile" | "equals" => Some(Some(V::I32(1))),
        _ => None,
    })
}

fn charset_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "forName" => {
            let n = sv(vm, args[0]).unwrap_or_else(|| "UTF-8".into());
            Some(Some(host(
                vm,
                "Ljava/nio/charset/Charset;",
                HostState::StrBuf(n),
            )))
        }
        "name" | "displayName" | "toString" => {
            let n = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
            .unwrap_or_else(|| "UTF-8".into());
            Some(Some(jstr(vm, &n)))
        }
        "defaultCharset" => Some(Some(host(
            vm,
            "Ljava/nio/charset/Charset;",
            HostState::StrBuf("UTF-8".into()),
        ))),
        "encode" | "decode" | "newEncoder" | "newDecoder" | "aliases" | "availableCharsets"
        | "isSupported" | "contains" | "canEncode" => {
            if name == "isSupported" || name == "canEncode" || name == "contains" {
                Some(Some(V::I32(1)))
            } else if name == "availableCharsets" {
                Some(Some(host(vm, "Ljava/util/Map;", HostState::Map(vec![]))))
            } else if name == "aliases" {
                Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![]))))
            } else {
                Some(Some(V::Ref(0)))
            }
        }
        _ => None,
    }
}

fn nio_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    // Backing byte[] handle for a `wrap`/`allocate` buffer.
    let backing = |vm: &Vm| -> Option<u32> {
        match vm.heap.get(this) {
            Some(Obj::Host {
                state: HostState::Slot(V::Ref(b)),
                ..
            }) if matches!(vm.heap.get(*b), Some(Obj::Array { .. })) => Some(*b),
            _ => None,
        }
    };
    let bpos = |vm: &Vm| i32v(pseudo_get(vm, this, P_BBUF_POS).unwrap_or(V::I32(0))).max(0) as usize;
    let little =
        |vm: &Vm| i32v(pseudo_get(vm, this, P_BBUF_ORD).unwrap_or(V::I32(0))) != 0;
    // Read `n` bytes at `pos`; None when out of bounds.
    let read_at = |vm: &Vm, pos: usize, n: usize| -> Option<Vec<u8>> {
        let b = backing(vm)?;
        match vm.heap.get(b) {
            Some(Obj::Array { data, .. }) => {
                if pos + n > data.len() {
                    None
                } else {
                    Some(
                        data[pos..pos + n]
                            .iter()
                            .map(|v| i32v(*v) as u8)
                            .collect(),
                    )
                }
            }
            _ => None,
        }
    };
    match name {
        "allocate" | "allocateDirect" => {
            let n = i32v(*args.get(if args.len() > 1 { 1 } else { 0 }).unwrap_or(&V::I32(0)));
            let buf = vm.heap.alloc(Obj::Array {
                elem: "B".into(),
                data: vec![V::I32(0); n.max(0) as usize],
            });
            Some(Some(host(
                vm,
                "Ljava/nio/ByteBuffer;",
                HostState::Slot(V::Ref(buf)),
            )))
        }
        "wrap" => {
            // wrap(byte[]) / wrap(byte[], off, len) — keep the array so
            // `array()` returns the same handle writes went into.
            let b = args.first().copied().unwrap_or(V::Ref(0));
            let v = host(vm, "Ljava/nio/ByteBuffer;", HostState::Slot(b));
            if let (V::Ref(h), true) = (v, args.len() > 2) {
                pseudo_set(vm, h, P_BBUF_POS, V::I32(i32v(args[1])));
            }
            Some(Some(v))
        }
        "order" => {
            let ord = match args.get(1) {
                Some(&V::Ref(bo)) => match vm.heap.get(bo) {
                    Some(Obj::Host {
                        state: HostState::Slot(v),
                        ..
                    }) => *v,
                    _ => V::I32(0),
                },
                _ => V::I32(0),
            };
            pseudo_set(vm, this, P_BBUF_ORD, ord);
            Some(Some(args[0]))
        }
        "array" => Some(Some(match vm.heap.get(this) {
            Some(Obj::Host {
                state: HostState::Slot(v),
                ..
            }) => *v,
            _ => V::Ref(0),
        })),
        "hasArray" | "hasRemaining" => Some(Some(V::I32(1))),
        "isDirect" | "isReadOnly" => Some(Some(V::I32(0))),
        "arrayOffset" => Some(Some(V::I32(0))),
        "capacity" | "limit" => {
            let n = backing(vm)
                .and_then(|b| match vm.heap.get(b) {
                    Some(Obj::Array { data, .. }) => Some(data.len()),
                    _ => None,
                })
                .unwrap_or(0);
            if args.len() > 1 {
                Some(Some(args[0]))
            } else {
                Some(Some(V::I32(n as i32)))
            }
        }
        "remaining" => {
            let n = backing(vm)
                .and_then(|b| match vm.heap.get(b) {
                    Some(Obj::Array { data, .. }) => Some(data.len()),
                    _ => None,
                })
                .unwrap_or(0);
            Some(Some(V::I32(n.saturating_sub(bpos(vm)) as i32)))
        }
        "position" => {
            if args.len() > 1 {
                pseudo_set(vm, this, P_BBUF_POS, args[1]);
                Some(Some(args[0]))
            } else {
                Some(Some(V::I32(bpos(vm) as i32)))
            }
        }
        "flip" | "clear" | "rewind" | "reset" => {
            pseudo_set(vm, this, P_BBUF_POS, V::I32(0));
            Some(Some(args[0]))
        }
        "mark" | "compact" | "slice" | "duplicate" | "asReadOnlyBuffer" => {
            Some(Some(args[0]))
        }
        "putInt" | "putLong" | "putShort" | "putDouble" | "putFloat" | "putChar" | "putByte"
        | "put" => {
            let pos0 = bpos(vm);
            let put = |vm: &mut Vm, pos: usize, bytes: &[u8]| {
                let mut b = bytes.to_vec();
                if little(vm) {
                    b.reverse();
                }
                if let Some(bh) = backing(vm) {
                    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                        for (i, x) in b.iter().enumerate() {
                            if let Some(slot) = data.get_mut(pos + i) {
                                *slot = V::I32(*x as i8 as i32);
                            }
                        }
                    }
                }
            };
            match name {
                "put" => {
                    if args.len() > 2 && matches!(args[1], V::I32(_) | V::I64(_)) {
                        // put(index, byte) — absolute, no cursor move.
                        let pos = i32v(args[1]).max(0) as usize;
                        let x = i32v(args[2]) as u8;
                        put(vm, pos, &[x]);
                    } else if args.len() > 2 {
                        // put(byte[], off, len)
                        let (src, off, len) = (
                            args[1],
                            i32v(args[2]).max(0) as usize,
                            i32v(args[3]).max(0) as usize,
                        );
                        let bytes: Vec<u8> = match src {
                            V::Ref(sh) => match vm.heap.get(sh) {
                                Some(Obj::Array { data, .. }) => data
                                    .iter()
                                    .skip(off)
                                    .take(len)
                                    .map(|v| i32v(*v) as u8)
                                    .collect(),
                                _ => Vec::new(),
                            },
                            _ => Vec::new(),
                        };
                        if let Some(bh) = backing(vm) {
                            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                                for (i, x) in bytes.iter().enumerate() {
                                    if let Some(slot) = data.get_mut(pos0 + i) {
                                        *slot = V::I32(*x as i8 as i32);
                                    }
                                }
                            }
                        }
                        pseudo_set(vm, this, P_BBUF_POS, V::I32((pos0 + len) as i32));
                    } else {
                        // put(byte[]) / put(srcBuffer) / put(byte).
                        let bytes: Vec<u8> = match args.get(1).copied() {
                            Some(V::Ref(sh)) => match vm.heap.get(sh) {
                                Some(Obj::Array { data, .. }) => {
                                    data.iter().map(|v| i32v(*v) as u8).collect()
                                }
                                _ => {
                                    // Another ByteBuffer: drain its
                                    // remaining bytes and move its cursor.
                                    let src_backing = match vm.heap.get(sh) {
                                        Some(Obj::Host {
                                            state: HostState::Slot(V::Ref(b)),
                                            ..
                                        }) if matches!(
                                            vm.heap.get(*b),
                                            Some(Obj::Array { .. })
                                        ) =>
                                        {
                                            Some(*b)
                                        }
                                        _ => None,
                                    };
                                    let mut v = Vec::new();
                                    if let Some(sb) = src_backing {
                                        let sp = i32v(
                                            pseudo_get(vm, sh, P_BBUF_POS)
                                                .unwrap_or(V::I32(0)),
                                        )
                                        .max(0)
                                            as usize;
                                        if let Some(Obj::Array { data, .. }) = vm.heap.get(sb) {
                                            v = data
                                                .iter()
                                                .skip(sp)
                                                .map(|x| i32v(*x) as u8)
                                                .collect();
                                        }
                                        let n = v.len();
                                        pseudo_set(
                                            vm,
                                            sh,
                                            P_BBUF_POS,
                                            V::I32((sp + n) as i32),
                                        );
                                    }
                                    v
                                }
                            },
                            v => vec![i32v(v.unwrap_or(V::I32(0))) as u8],
                        };
                        let len = bytes.len();
                        // Byte-order applies to multi-byte primitives,
                        // not bulk copies — write through directly.
                        if let Some(bh) = backing(vm) {
                            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                                for (i, x) in bytes.iter().enumerate() {
                                    if let Some(slot) = data.get_mut(pos0 + i) {
                                        *slot = V::I32(*x as i8 as i32);
                                    }
                                }
                            }
                        }
                        pseudo_set(vm, this, P_BBUF_POS, V::I32((pos0 + len) as i32));
                    }
                }
                _ => {
                    let v = args.get(1).copied().unwrap_or(V::I32(0));
                    let bytes: Vec<u8> = match name {
                        "putInt" => i32v(v).to_be_bytes().to_vec(),
                        "putLong" => i64v(v).to_be_bytes().to_vec(),
                        "putDouble" => f64v(v).to_be_bytes().to_vec(),
                        "putFloat" => (f64v(v) as f32).to_be_bytes().to_vec(),
                        "putShort" | "putChar" => (i32v(v) as u16).to_be_bytes().to_vec(),
                        _ => vec![i32v(v) as u8],
                    };
                    // Index form putInt(i, v) etc.: 3 args.
                    let (pos, adv) = if args.len() > 2 {
                        (i32v(args[1]).max(0) as usize, 0)
                    } else {
                        (pos0, bytes.len())
                    };
                    let v2 = if args.len() > 2 { args[2] } else { v };
                    let bytes = if args.len() > 2 {
                        match name {
                            "putInt" => i32v(v2).to_be_bytes().to_vec(),
                            "putLong" => i64v(v2).to_be_bytes().to_vec(),
                            "putDouble" => f64v(v2).to_be_bytes().to_vec(),
                            "putFloat" => (f64v(v2) as f32).to_be_bytes().to_vec(),
                            "putShort" | "putChar" => {
                                (i32v(v2) as u16).to_be_bytes().to_vec()
                            }
                            _ => vec![i32v(v2) as u8],
                        }
                    } else {
                        bytes
                    };
                    put(vm, pos, &bytes);
                    if adv > 0 {
                        pseudo_set(
                            vm,
                            this,
                            P_BBUF_POS,
                            V::I32((pos0 + bytes.len()) as i32),
                        );
                    }
                }
            }
            Some(Some(args[0]))
        }
        "get" | "getInt" | "getLong" | "getShort" | "getDouble" | "getFloat" | "getChar"
        | "getByte" => {
            let size = match name {
                "getInt" | "getFloat" => 4,
                "getLong" | "getDouble" => 8,
                "getShort" | "getChar" => 2,
                _ => 1,
            };
            let pos = if args.len() > 1 && matches!(args[1], V::I32(_) | V::I64(_)) {
                i32v(args[1]).max(0) as usize
            } else {
                bpos(vm)
            };
            if name == "get" && args.len() > 1 && matches!(args[1], V::Ref(_)) {
                // get(byte[], off, len) — bulk read.
                let (dst, off, len) = (
                    args[1],
                    args.get(2).map(|v| i32v(*v).max(0) as usize).unwrap_or(0),
                    args.get(3)
                        .map(|v| i32v(*v).max(0) as usize)
                        .unwrap_or(usize::MAX),
                );
                if let Some(bytes) = read_at(vm, pos, len.min(usize::MAX / 2).min(len)) {
                    let n = bytes.len();
                    if let V::Ref(dh) = dst {
                        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(dh) {
                            for (i, x) in bytes.iter().enumerate() {
                                if let Some(slot) = data.get_mut(off + i) {
                                    *slot = V::I32(*x as i8 as i32);
                                }
                            }
                        }
                    }
                    pseudo_set(vm, this, P_BBUF_POS, V::I32((pos + n) as i32));
                }
                return Some(Some(args[0]));
            }
            let mut bytes = read_at(vm, pos, size).unwrap_or_else(|| vec![0; size]);
            if little(vm) {
                bytes.reverse();
            }
            if args.len() == 1 {
                pseudo_set(vm, this, P_BBUF_POS, V::I32((pos + size) as i32));
            }
            let v = match name {
                "getInt" => V::I32(i32::from_be_bytes(
                    bytes[..4].try_into().unwrap_or([0; 4]),
                )),
                "getLong" => V::I64(i64::from_be_bytes(
                    bytes[..8].try_into().unwrap_or([0; 8]),
                )),
                "getDouble" => V::F64(f64::from_be_bytes(
                    bytes[..8].try_into().unwrap_or([0; 8]),
                )),
                "getFloat" => V::F32(f32::from_be_bytes(
                    bytes[..4].try_into().unwrap_or([0; 4]),
                )),
                "getShort" => V::I32(i16::from_be_bytes(
                    bytes[..2].try_into().unwrap_or([0; 2]),
                ) as i32),
                "getChar" => V::I32(u16::from_be_bytes(
                    bytes[..2].try_into().unwrap_or([0; 2]),
                ) as i32),
                _ => V::I32(bytes[0] as i8 as i32),
            };
            Some(Some(v))
        }
        _ => None,
    }
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, b| (a << 8) | *b as u32) << (8 * (3 - chunk.len()));
        let pad = 3 - chunk.len();
        for i in 0..4 {
            if i < 4 - pad {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => 64,
        }
    };
    let mut out = vec![];
    let mut buf = 0u32;
    let mut nbits = 0;
    for c in s.bytes() {
        let v = val(c);
        if v == 64 {
            continue;
        }
        buf = (buf << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((buf >> nbits) as u8);
        }
    }
    out
}

fn base64_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getEncoder" | "getUrlEncoder" | "getMimeEncoder" | "getDecoder" | "getUrlDecoder"
        | "getMimeDecoder" => {
            let d = if name.contains("Decoder") {
                "Ljava/util/Base64$Decoder;"
            } else {
                "Ljava/util/Base64$Encoder;"
            };
            Some(Some(host(vm, d, HostState::Empty)))
        }
        "encode" | "encodeToString" | "encodeToByteArray" => {
            let data = match args[0] {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.iter().map(|v| i32v(*v) as u8).collect(),
                    Some(Obj::Str(t)) => t.bytes().collect(),
                    Some(Obj::Host {
                        state: HostState::StrBuf(t),
                        ..
                    }) => t.bytes().collect(),
                    _ => vec![],
                },
                _ => vec![],
            };
            let enc = b64_encode(&data);
            if name == "encodeToByteArray" || name == "encode" {
                let vs: Vec<V> = enc.bytes().map(|b| V::I32(b as i32)).collect();
                Some(Some(arr(vm, "B", vs)))
            } else {
                Some(Some(jstr(vm, &enc)))
            }
        }
        "decode" => {
            let t = match args[0] {
                V::Ref(h) => s(vm, h).unwrap_or_default(),
                _ => String::new(),
            };
            let dec = b64_decode(&t);
            let vs: Vec<V> = dec.iter().map(|b| V::I32(*b as i8 as i32)).collect();
            Some(Some(arr(vm, "B", vs)))
        }
        "wrap" => Some(args.first().copied()),
        _ => None,
    }
}

fn misc_fmt(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    if class == "Ljava/util/Locale;" {
        return match name {
            "getDefault" | "getDefault0" | "forLanguageTag" | "ROOT" => Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("en-US".into()),
            )),
            "getLanguage" => Some(jstr(vm, "en")),
            "getCountry" | "getVariant" | "getScript" => Some(jstr(vm, "US")),
            "toString" | "toLanguageTag" | "getDisplayName" | "getDisplayLanguage"
            | "getDisplayCountry" => {
                let t = s(
                    vm,
                    match args[0] {
                        V::Ref(h) => h,
                        _ => 0,
                    },
                )
                .unwrap_or_else(|| "en-US".into());
                Some(jstr(vm, &t))
            }
            "getAvailableLocales" | "getISOCountries" | "getISOLanguages" => {
                Some(arr(vm, STRING, vec![]))
            }
            _ => return None,
        }
        .map(Some);
    }
    if class == "Ljava/util/Formatter;" {
        return match name {
            "format" => {
                let fmt = sv(vm, args[1]).unwrap_or_default();
                let flat: Vec<V> = args[2..].to_vec();
                let out = simple_format(vm, &fmt, &flat);
                if let V::Ref(h) = args[0] {
                    if let Some(Obj::Host {
                        state: HostState::StrBuf(b),
                        ..
                    }) = vm.heap.get_mut(h)
                    {
                        b.push_str(&out);
                    }
                }
                Some(args[0])
            }
            "toString" | "out" => {
                let t = s(
                    vm,
                    match args[0] {
                        V::Ref(h) => h,
                        _ => 0,
                    },
                )
                .unwrap_or_default();
                Some(jstr(vm, &t))
            }
            "flush" | "close" | "locale" => Some(args[0]),
            _ => return None,
        }
        .map(Some);
    }
    if class == "Ljava/util/Scanner;" {
        return match name {
            "next" | "nextLine" => Some(V::Ref(0)),
            "hasNext" | "hasNextLine" | "hasNextInt" | "hasNextLong" | "hasNextDouble"
            | "hasNextBoolean" => Some(V::I32(0)),
            "nextInt" | "nextLong" | "nextDouble" | "nextFloat" | "nextShort" | "nextByte" => {
                Some(V::I64(0))
            }
            "useDelimiter" | "useLocale" | "useRadix" | "reset" | "close" | "skip"
            | "findInLine" | "findWithinHorizon" => {
                if matches!(name, "skip" | "findInLine" | "findWithinHorizon") {
                    Some(V::Ref(0))
                } else {
                    Some(args[0])
                }
            }
            "delimiter" | "locale" | "radix" | "match" | "ioException" => Some(V::Ref(0)),
            "tokens" => Some(host(vm, "Ljava/util/stream/Stream;", HostState::Empty)),
            "remove" => None,
            _ => return None,
        }
        .map(Some);
    }
    if class == "Ljava/text/MessageFormat;" {
        return match name {
            "format" => {
                let pat = sv(vm, args[0]).unwrap_or_default();
                let mut out = pat;
                for (i, v) in args[1..].iter().enumerate() {
                    let t = v_to_s(vm, *v);
                    out = out.replace(&format!("{{{i}}}"), &t);
                }
                Some(jstr(vm, &out))
            }
            _ => return None,
        }
        .map(Some);
    }
    let _ = (class, name, args);
    None
}

/// The guest path a stream ctor argument points at: `File` (Path host),
/// `String`, or `Uri`/`file:` string. None for other objects.
fn stream_arg_path(vm: &Vm, v: V) -> Option<String> {
    let V::Ref(h) = v else { return None };
    let raw = match vm.heap.get(h) {
        Some(Obj::Str(s)) => s.clone(),
        Some(Obj::Host {
            state:
                HostState::Path(s) | HostState::StrBuf(s) | HostState::Regex(s),
            ..
        }) => s.clone(),
        _ => return None,
    };
    Some(raw.strip_prefix("file://").unwrap_or(&raw).to_string())
}
