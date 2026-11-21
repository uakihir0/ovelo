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
            stream_call(vm, name, args)
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

fn stream_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
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
                    return Some(Some(if *pos < data.len() {
                        let b = data[*pos] as i32;
                        *pos += 1;
                        V::I32(b)
                    } else {
                        V::I32(-1)
                    }));
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
                    return Some(Some(V::I32(-1)));
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
                return Some(Some(V::I32(n as i32)));
            }
            "available" => {
                return Some(Some(V::I32((data.len() - *pos) as i32)));
            }
            "skip" => {
                let n = i64v(args.get(1).copied().unwrap_or(V::I64(0))).max(0) as usize;
                let took = n.min(data.len() - *pos);
                *pos += took;
                return Some(Some(V::I64(took as i64)));
            }
            "reset" => {
                *pos = 0;
                return Some(None);
            }
            "mark" | "close" => return Some(None),
            "markSupported" => return Some(Some(V::I32(1))),
            _ => {}
        }
    }
    let _ = vm;
    match name {
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
    }
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
    match name {
        "allocate" | "allocateDirect" | "wrap" | "order" => {
            let n = i32v(*args.last().unwrap_or(&V::I32(0)));
            let st = if args.len() > 1 && matches!(args[0], V::Ref(_)) {
                HostState::Slot(args[0])
            } else {
                HostState::List(vec![V::I32(0); n.max(0) as usize])
            };
            Some(Some(host(vm, "Ljava/nio/ByteBuffer;", st)))
        }
        "capacity" | "limit" | "remaining" | "position" => {
            let n = match args[0] {
                V::Ref(h) => list_ref(vm, h).map(|l| l.len()).unwrap_or(0),
                _ => 0,
            };
            Some(Some(V::I32(n as i32)))
        }
        "get" | "put" | "flip" | "clear" | "rewind" | "mark" | "reset" | "compact" | "slice"
        | "duplicate" | "asReadOnlyBuffer" | "array" | "hasArray" | "arrayOffset" | "isDirect"
        | "isReadOnly" | "hasRemaining" => {
            if name.starts_with("is") || name == "hasArray" || name == "hasRemaining" {
                Some(Some(V::I32(
                    (name == "hasRemaining" || name == "hasArray") as i32,
                )))
            } else if name == "array" {
                Some(Some(V::Ref(0)))
            } else {
                Some(Some(args[0]))
            }
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
