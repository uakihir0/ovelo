//! java.io / java.net / java.nio / regex / SimpleDateFormat / org.json.

use super::rx::{full_match, Rx};
use super::*;
use crate::value::{DigestState, HostState, Obj, V};
use hmac::Mac as _;
use sha2::Digest as _;

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if class.starts_with("Ljava/nio/file/") {
        nio_file_call(vm, class, name, args)?
    } else if desc_is_a(class, "Ljava/io/PrintStream;") || desc_is_a(class, "Ljava/io/PrintWriter;")
    {
        print_call(vm, name, sig, args)
    } else if class == "Ljava/security/MessageDigest;" {
        digest_call(vm, name, args)?
    } else if class == "Ljavax/crypto/Mac;" {
        mac_call(vm, name, args)?
    } else if class == "Ljavax/crypto/spec/SecretKeySpec;" {
        key_spec_call(vm, name, args)?
    } else if class == "Ljava/util/jar/JarFile;" || class == "Ljava/util/zip/ZipFile;" {
        zip_file_call(vm, class, name, args)
    } else if class == "Ljava/util/jar/JarEntry;" || class == "Ljava/util/zip/ZipEntry;" {
        zip_entry_call(vm, name, args)
    } else if class == "Landroid/util/AtomicFile;" {
        atomic_file_call(vm, name, args)?
    } else if class == "Ljava/security/CodeSigner;"
        || class == "Ljava/security/cert/CertPath;"
        || class == "Ljava/security/cert/Certificate;"
        || class == "Ljava/security/cert/X509Certificate;"
    {
        cert_call(vm, class, name, args)
    } else if class.ends_with("FileOutputStream;") && name == "<init>" {
        let arg = args.get(1).copied().unwrap_or(V::Ref(0));
        let append = args.get(2).is_some_and(|v| i32v(*v) != 0);
        if let Some(path) = stream_arg_path(vm, arg) {
            let path = fspath(vm, &path);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(!append)
                .append(append)
                .open(path)
                .ok();
            if let Some(file) = file {
                replace_host_state(vm, args[0], class, HostState::File(file));
            }
        }
        Some(None)
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
    } else if class == "Ljava/io/ObjectOutputStream;" {
        object_output_call(vm, name, args)?
    } else if class == "Ljava/io/ObjectInputStream;" {
        object_input_call(vm, name, args)?
    } else if class == "Ljava/io/ByteArrayInputStream;" && name == "<init>" {
        // ByteArrayInputStream(byte[]) / (byte[], off, len): store the
        // payload so reads serve the real bytes.
        let arg = args.get(1).copied().unwrap_or(V::Ref(0));
        let mut bytes: Vec<u8> = match arg {
            V::Ref(h) => match vm.heap.get(h) {
                Some(Obj::Array { data, .. }) => data.iter().map(|v| i32v(*v) as u8).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if args.len() > 3 {
            let off = i64v(args[2]).max(0) as usize;
            let len = i64v(args[3]).max(0) as usize;
            bytes = bytes.into_iter().skip(off).take(len).collect();
        }
        if let Some(V::Ref(this)) = args.first() {
            if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(*this) {
                *state = HostState::Bytes(std::rc::Rc::new(bytes), 0);
            }
        }
        Some(None)
    } else if name == "<init>" && class.ends_with("Reader;") {
        // Char-stream constructors: materialize the source into a
        // `Chars` payload (StringReader/CharArrayReader), eagerly
        // decode a byte-backed stream (InputStreamReader), or keep
        // the wrapped reader in `in` for delegation (wrapper readers
        // like BufferedReader/FilterReader).
        if let Some(V::Ref(this)) = args.first() {
            let this = *this;
            let arg = args.get(1).copied().unwrap_or(V::Ref(0));
            let chars: Option<Vec<u16>> = if class == "Ljava/io/StringReader;" {
                let s = sv(vm, arg).unwrap_or_default();
                if let Some(p) = std::env::var_os("OVELO_DUMP_READER") {
                    use std::io::Write as _;
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&p)
                        .map(|mut f| {
                            let _ = write!(
                                f,
                                "=== StringReader #{} len={} tail={:?}\n{}\n",
                                this,
                                s.chars().count(),
                                s.chars().rev().take(80).collect::<String>(),
                                s
                            );
                        });
                }
                Some(s.encode_utf16().collect())
            } else if class == "Ljava/io/CharArrayReader;" {
                match arg {
                    V::Ref(h) => match vm.heap.get(h) {
                        Some(Obj::Array { data, .. }) => {
                            Some(data.iter().map(|v| i32v(*v) as u16).collect())
                        }
                        _ => Some(Vec::new()),
                    },
                    _ => Some(Vec::new()),
                }
            } else if class == "Ljava/io/InputStreamReader;" {
                match arg {
                    V::Ref(h) => match vm.heap.get(h) {
                        Some(Obj::Host {
                            state: HostState::Bytes(b, p),
                            ..
                        }) => {
                            let s = String::from_utf8_lossy(&b[*p..]).into_owned();
                            Some(s.encode_utf16().collect())
                        }
                        _ => None,
                    },
                    _ => None,
                }
            } else {
                None
            };
            match chars {
                Some(c) => {
                    if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(this) {
                        *state = HostState::Chars(std::rc::Rc::new(c), 0, 0);
                    }
                }
                None => {
                    // Wrapper reader: remember the delegate. P_DATA is
                    // authoritative because JDK-private `in` fields are
                    // commonly absent from the app's dex field table.
                    pseudo_set(vm, this, P_DATA, arg);
                    for owner in ["Ljava/io/FilterReader;", "Ljava/io/Reader;"] {
                        field_set(vm, this, owner, "in", arg);
                    }
                }
            }
        }
        Some(None)
    } else if class == "Ljava/io/ByteArrayOutputStream;" && name == "<init>" {
        // The output side of the Bytes payload: writes append.
        if let Some(V::Ref(this)) = args.first() {
            if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(*this) {
                *state = HostState::Bytes(std::rc::Rc::new(Vec::new()), 0);
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
        json_obj(vm, name, args)?
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
    } else if class == "Ljava/net/IDN;" {
        match name {
            // IDN.toASCII: ASCII input passes through unchanged; the
            // punycode path only matters for non-ASCII domains.
            "toASCII" | "toUnicode" => Some(args.first().copied().unwrap_or(V::Ref(0))),
            _ => return Ok(None),
        }
        .map(Some)
    } else if class == "Ljava/net/URL;" || class == "Ljava/net/URI;" {
        url_call(vm, name, args)?
    } else if class == "Ljava/net/URLConnection;" || desc_is_a(class, "Ljava/net/URLConnection;") {
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
            "getResponseMessage" | "getContentType" | "getContentEncoding" | "getHeaderField"
            | "getRequestProperty" | "getURL" => Some(Some(V::Ref(0))),
            "getContentLength"
            | "getContentLengthLong"
            | "getDate"
            | "getExpiration"
            | "getLastModified"
            | "getHeaderFieldInt"
            | "getHeaderFieldLong" => Some(Some(V::I64(0))),
            "getInputStream" => Some(Some(host(vm, "Ljava/io/InputStream;", HostState::Empty))),
            "getOutputStream" => Some(Some(host(vm, "Ljava/io/OutputStream;", HostState::Empty))),
            "getHeaderFields" => Some(Some(host(vm, "Ljava/util/Map;", HostState::Map(vec![])))),
            _ => None,
        }
    } else if desc_is_a(class, "Ljava/nio/charset/Charset;")
        || class == "Ljava/nio/charset/StandardCharsets;"
        || class == "Ljava/nio/charset/CharsetDecoder;"
        || class == "Ljava/nio/charset/CharsetEncoder;"
        || class == "Ljava/nio/charset/CoderResult;"
        || class == "Ljava/nio/charset/CodingErrorAction;"
    {
        charset_call(vm, class, name, args)
    } else if desc_is_a(class, "Ljava/nio/Buffer;") || class.starts_with("Ljava/nio/") {
        nio_call(vm, class, name, args)
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
    })
}

// ---------------------------------------------------------------------
// PrintStream / PrintWriter — mirror to stderr

fn print_call(vm: &mut Vm, name: &str, _sig: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "print" | "println" => {
            let mut t = if args.len() > 1 {
                v_to_s(vm, args[1])
            } else {
                String::new()
            };
            if name == "println" {
                t.push('\n');
            }
            // Forward into the wrapped writer's buffer when there is
            // one; a non-buffered sink (e.g. PrintWriter(System.out))
            // mirrors to stderr instead of dropping the text.
            if !push_strbuf(vm, h_of(args[0]), &t) {
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
        "write" | "append" => {
            // PrintWriter(Writer) forwards — the wrapped writer is under
            // P_DATA so prints land in its buffer.
            if let Some(&a1) = args.get(1) {
                let t = writer_text(
                    vm,
                    a1,
                    args.get(2).copied(),
                    args.get(3).copied(),
                    name == "write",
                );
                if !push_strbuf(vm, h_of(args[0]), &t) {
                    eprint!("{t}");
                }
            }
            if name == "append" {
                Some(Some(args[0]))
            } else {
                Some(None)
            }
        }
        "flush" | "close" | "checkError" => {
            if name == "checkError" {
                Some(Some(V::I32(0)))
            } else {
                Some(None)
            }
        }
        "<init>" => {
            if let Some(&V::Ref(w)) = args.get(1) {
                pseudo_set(vm, h_of(args[0]), P_DATA, V::Ref(w));
            }
            Some(None)
        }
        _ => None,
    }
}

/// Follow the P_DATA chain of wrapper writers (PrintWriter,
/// BufferedWriter, OutputStreamWriter) to the innermost target.
fn wrapped_writer(vm: &mut Vm, this: u32) -> Option<u32> {
    let mut h = this;
    for _ in 0..4 {
        match pseudo_get(vm, h, P_DATA) {
            Some(V::Ref(n)) if n != h => h = n,
            _ => break,
        }
    }
    if h != this {
        Some(h)
    } else {
        None
    }
}

fn h_of(v: V) -> u32 {
    match v {
        V::Ref(h) => h,
        _ => 0,
    }
}

/// Append `t` to the innermost wrapped writer's StrBuf — returns false
/// when the chain ends somewhere that cannot hold text.
fn push_strbuf(vm: &mut Vm, this: u32, t: &str) -> bool {
    let Some(w) = wrapped_writer(vm, this) else {
        return false;
    };
    match vm.heap.get_mut(w) {
        Some(Obj::Host {
            state: HostState::StrBuf(b),
            ..
        }) => {
            b.push_str(t);
            true
        }
        _ => false,
    }
}

/// The text a writer's `write`/`append` argument denotes: an int is a
/// single UTF-16 unit, a char[] is decoded as UTF-16 (surrogate pairs
/// survive), anything else is its CharSequence view. `lo_v`/`hi_v` are
/// the optional (off,len) bounds of `write` or the (start,end) bounds
/// of `append` — `len_mode` picks which.
fn writer_text(vm: &mut Vm, v: V, lo_v: Option<V>, hi_v: Option<V>, len_mode: bool) -> String {
    match v {
        V::I32(c) => char::from_u32(c as u32)
            .map(|c| c.to_string())
            .unwrap_or_default(),
        V::Ref(ah) if matches!(vm.heap.get(ah), Some(Obj::Array { .. })) => {
            let Some(Obj::Array { data, .. }) = vm.heap.get(ah) else {
                return String::new();
            };
            let lo = lo_v
                .map(|v| i64v(v).max(0) as usize)
                .unwrap_or(0)
                .min(data.len());
            let hi = if len_mode {
                hi_v.map(|v| lo + i64v(v).max(0) as usize)
            } else {
                hi_v.map(|v| i64v(v).max(0) as usize)
            }
            .unwrap_or(data.len())
            .min(data.len());
            let units: Vec<u16> = data[lo..hi.max(lo)]
                .iter()
                .map(|v| i64v(*v) as u16)
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => {
            let t = v_to_s(vm, v);
            let n = t.chars().count();
            let lo = lo_v.map(|v| i64v(v).max(0) as usize).unwrap_or(0).min(n);
            let hi = if len_mode {
                hi_v.map(|v| lo + i64v(v).max(0) as usize)
            } else {
                hi_v.map(|v| i64v(v).max(0) as usize)
            }
            .unwrap_or(n)
            .min(n);
            if lo == 0 && hi == n {
                t
            } else {
                t.chars().skip(lo).take(hi.max(lo) - lo).collect()
            }
        }
    }
}

const OVELO_SER_MAGIC: &[u8; 8] = b"OVLSER1\0";

fn object_output_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first().copied() {
        Some(V::Ref(h)) => h,
        _ => 0,
    };
    Ok(Some(match name {
        "<init>" => {
            pseudo_set(vm, this, P_DATA, args.get(1).copied().unwrap_or(V::Ref(0)));
            None
        }
        "writeObject" => {
            let obj = args.get(1).copied().unwrap_or(V::Ref(0));
            let h = match obj {
                V::Ref(h) => h,
                _ => 0,
            };
            let mut bytes = OVELO_SER_MAGIC.to_vec();
            bytes.extend_from_slice(&h.to_le_bytes());
            let arr_v = byte_arr(vm, &bytes);
            if let Some(sink) = pseudo_get(vm, this, P_DATA) {
                let write_args = [sink, arr_v, V::I32(0), V::I32(bytes.len() as i32)];
                let _ = stream_call(vm, "write", &write_args)?;
            }
            None
        }
        "flush" | "close" => {
            if let Some(sink) = pseudo_get(vm, this, P_DATA) {
                let close_args = [sink];
                let _ = stream_call(vm, name, &close_args)?;
            }
            None
        }
        _ => return Ok(None),
    }))
}

fn object_input_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first().copied() {
        Some(V::Ref(h)) => h,
        _ => 0,
    };
    Ok(Some(match name {
        "<init>" => {
            pseudo_set(vm, this, P_DATA, args.get(1).copied().unwrap_or(V::Ref(0)));
            None
        }
        "readObject" => {
            let source = pseudo_get(vm, this, P_DATA).unwrap_or(V::Ref(0));
            let bytes = match source {
                V::Ref(sh) => match vm.heap.get(sh) {
                    Some(Obj::Host {
                        state: HostState::Bytes(b, p),
                        ..
                    }) => b[*p..].to_vec(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if bytes.len() >= 12 && bytes[..8] == OVELO_SER_MAGIC[..] {
                let h = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
                Some(shallow_clone_object(vm, h))
            } else {
                Some(V::Ref(0))
            }
        }
        "close" => None,
        _ => return Ok(None),
    }))
}

/// Serialization-roundtrip object copy used by ObjectInputStream above.
/// Copying the instance field map gives the deep-clone callers the
/// object identity and assignability they need while immutable leaves
/// (strings, UUID placeholders) can safely stay shared.
fn shallow_clone_object(vm: &mut Vm, h: u32) -> V {
    let cloned = match vm.heap.get(h) {
        Some(Obj::Instance { class_idx, fields }) => Some((*class_idx, fields.clone())),
        _ => None,
    };
    if let Some((class_idx, fields)) = cloned {
        return V::Ref(vm.heap.alloc(Obj::Instance { class_idx, fields }));
    }
    V::Ref(h)
}

/// android.util.AtomicFile. Aegis uses this for vault persistence; the
/// compatibility implementation writes the base file directly (no backup
/// rename dance), which preserves the API contract apps rely on.
fn atomic_file_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first().copied() {
        Some(V::Ref(h)) => h,
        _ => 0,
    };
    let path_of = |vm: &Vm| -> String {
        match vm.heap.get(this) {
            Some(Obj::Host {
                state: HostState::Path(p),
                ..
            }) => p.clone(),
            _ => pseudo_get(vm, this, P_DATA)
                .and_then(|v| stream_arg_path(vm, v))
                .unwrap_or_default(),
        }
    };
    Ok(Some(match name {
        "<init>" => {
            let file = args.get(1).copied().unwrap_or(V::Ref(0));
            if let Some(path) = stream_arg_path(vm, file) {
                replace_host_state(
                    vm,
                    args[0],
                    "Landroid/util/AtomicFile;",
                    HostState::Path(path),
                );
                pseudo_set(vm, this, P_DATA, file);
            }
            None
        }
        "getBaseFile" => Some(pseudo_get(vm, this, P_DATA).unwrap_or(V::Ref(0))),
        "startWrite" => {
            let guest = path_of(vm);
            let path = fspath(vm, &guest);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Preserve the last complete file until the replacement is
            // finished. AtomicFile's contract is exactly this recovery
            // dance; truncating in place would lose the vault on error.
            let bak = fspath(vm, &format!("{guest}.bak"));
            if path.exists() {
                let _ = std::fs::remove_file(&bak);
                if std::fs::rename(&path, &bak).is_err() {
                    return Err(vm.throw_builtin(
                        "Ljava/io/IOException;",
                        "AtomicFile backup rename failed",
                    ));
                }
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .ok();
            let Some(file) = file else {
                return Err(
                    vm.throw_builtin("Ljava/io/IOException;", "AtomicFile startWrite failed")
                );
            };
            Some(host(
                vm,
                "Ljava/io/FileOutputStream;",
                HostState::File(file),
            ))
        }
        "finishWrite" => {
            if let Some(V::Ref(h)) = args.get(1).copied() {
                let close_args = [V::Ref(h)];
                let _ = stream_call(vm, "close", &close_args)?;
            }
            let guest = path_of(vm);
            let _ = std::fs::remove_file(fspath(vm, &format!("{guest}.bak")));
            None
        }
        "failWrite" => {
            if let Some(V::Ref(h)) = args.get(1).copied() {
                let close_args = [V::Ref(h)];
                let _ = stream_call(vm, "close", &close_args)?;
            }
            let guest = path_of(vm);
            let path = fspath(vm, &guest);
            let bak = fspath(vm, &format!("{guest}.bak"));
            let _ = std::fs::remove_file(&path);
            if bak.exists() {
                let _ = std::fs::rename(&bak, &path);
            }
            None
        }
        "readFully" => {
            let guest = path_of(vm);
            let bytes = std::fs::read(fspath(vm, &guest)).unwrap_or_default();
            Some(byte_arr(vm, &bytes))
        }
        "delete" => {
            let guest = path_of(vm);
            let _ = std::fs::remove_file(fspath(vm, &guest));
            let _ = std::fs::remove_file(fspath(vm, &format!("{guest}.bak")));
            None
        }
        _ => return Ok(None),
    }))
}

fn stream_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    // ByteArrayOutputStream accumulates into the same Bytes payload —
    // handled before the mutable borrow below because the write source
    // and the returned array/string need second heap accesses.
    let is_baos = matches!(
        vm.heap.get(this),
        Some(Obj::Host {
            desc,
            state: HostState::Bytes(..),
            ..
        }) if desc == "Ljava/io/ByteArrayOutputStream;"
    );
    if is_baos {
        match name {
            "write" => {
                let mut chunk: Vec<u8> = Vec::new();
                match args.get(1) {
                    Some(&V::Ref(bh)) => {
                        if let Some(Obj::Array { data: src, .. }) = vm.heap.get(bh) {
                            let off = args.get(2).map(|v| i64v(*v).max(0) as usize).unwrap_or(0);
                            let want = args
                                .get(3)
                                .map(|v| i64v(*v).max(0) as usize)
                                .unwrap_or(src.len());
                            chunk = src
                                .iter()
                                .skip(off)
                                .take(want)
                                .map(|v| i64v(*v) as u8)
                                .collect();
                        }
                    }
                    Some(v) => chunk.push(i64v(*v) as u8),
                    _ => {}
                }
                if let Some(Obj::Host {
                    state: HostState::Bytes(data, _),
                    ..
                }) = vm.heap.get_mut(this)
                {
                    std::rc::Rc::make_mut(data).extend_from_slice(&chunk);
                }
                return Ok(Some(None));
            }
            "toByteArray" => {
                let bytes = match vm.heap.get(this) {
                    Some(Obj::Host {
                        state: HostState::Bytes(d, _),
                        ..
                    }) => d.clone(),
                    _ => std::rc::Rc::new(Vec::new()),
                };
                let out: Vec<V> = bytes.iter().map(|b| V::I32(*b as i8 as i32)).collect();
                return Ok(Some(Some(arr(vm, "B", out))));
            }
            "size" => {
                let n = match vm.heap.get(this) {
                    Some(Obj::Host {
                        state: HostState::Bytes(d, _),
                        ..
                    }) => d.len() as i32,
                    _ => 0,
                };
                return Ok(Some(Some(V::I32(n))));
            }
            "reset" => {
                if let Some(Obj::Host {
                    state: HostState::Bytes(d, p),
                    ..
                }) = vm.heap.get_mut(this)
                {
                    std::rc::Rc::make_mut(d).clear();
                    *p = 0;
                }
                return Ok(Some(None));
            }
            "toString" => {
                let bytes = match vm.heap.get(this) {
                    Some(Obj::Host {
                        state: HostState::Bytes(d, _),
                        ..
                    }) => d.clone(),
                    _ => std::rc::Rc::new(Vec::new()),
                };
                let t = String::from_utf8_lossy(&bytes).into_owned();
                return Ok(Some(Some(jstr(vm, &t))));
            }
            "writeTo" | "flush" | "close" => return Ok(Some(None)),
            _ => {}
        }
    }
    // Byte-backed streams (raw resources, assets): real reads.
    if let Some(Obj::Host {
        desc: stream_desc,
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
                if crate::dbg_flag("OVELO_STREAMDBG") {
                    eprintln!(
                        "[sdbg] read {stream_desc}#{this} pos-{} n={} want={} off={} args={:?} head={:?}",
                        *pos,
                        n,
                        want,
                        off,
                        args,
                        chunk.iter().take(24).collect::<Vec<_>>()
                    );
                }
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
    // Char-backed readers (StringReader & friends): real char reads.
    if let Some(Obj::Host {
        state: HostState::Chars(data, pos, mark),
        ..
    }) = vm.heap.get_mut(this)
    {
        match name {
            "read" => {
                if args.len() == 1 {
                    return Ok(Some(Some(if *pos < data.len() {
                        let c = data[*pos] as i32;
                        *pos += 1;
                        V::I32(c)
                    } else {
                        V::I32(-1)
                    })));
                }
                // read(char[]) / read(char[], off, len)
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
                let chunk: Vec<u16> = data[*pos..*pos + n].to_vec();
                *pos += n;
                if let Some(Obj::Array { data: dst, .. }) = vm.heap.get_mut(buf) {
                    for (i, c) in chunk.iter().enumerate() {
                        if let Some(slot) = dst.get_mut(off + i) {
                            *slot = V::I32(*c as i32);
                        }
                    }
                }
                return Ok(Some(Some(V::I32(n as i32))));
            }
            "available" | "ready" => {
                return Ok(Some(Some(V::I32(if *pos < data.len() { 1 } else { 0 }))));
            }
            "skip" => {
                let n = i64v(args.get(1).copied().unwrap_or(V::I64(0))).max(0) as usize;
                let took = n.min(data.len() - *pos);
                *pos += took;
                return Ok(Some(Some(V::I64(took as i64))));
            }
            "mark" => {
                *mark = *pos;
                return Ok(Some(None));
            }
            "reset" => {
                *pos = *mark;
                return Ok(Some(None));
            }
            "markSupported" => return Ok(Some(Some(V::I32(1)))),
            "close" => return Ok(Some(None)),
            _ => {}
        }
    }
    // `FilterInputStream`-style wrappers: the java methods we don't
    // model delegate to the protected `in` field (stored by the
    // `<init>` shim). A guest IOException propagates so the app's own
    // catch blocks see it.
    if matches!(
        name,
        "read"
            | "readLine"
            | "available"
            | "skip"
            | "mark"
            | "reset"
            | "markSupported"
            | "ready"
            | "close"
    ) {
        let inner = pseudo_get(vm, this, P_DATA)
            .or_else(|| {
                [
                    "Ljava/io/FilterInputStream;",
                    "Ljava/io/FilterReader;",
                    "Ljava/io/Reader;",
                    "Ljava/io/BufferedReader;",
                    "Ljava/io/InputStreamReader;",
                ]
                .iter()
                .find_map(|owner| field_get(vm, this, owner, "in"))
            })
            .and_then(|v| match v {
                V::Ref(h) => Some(h),
                _ => None,
            });
        if let Some(inner) = inner {
            if inner != 0 && inner != this {
                if crate::dbg_flag("OVELO_STREAMDBG") {
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
                    (i64v(args[2]).max(0) as usize, i64v(args[3]).max(0) as usize)
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
    // `<init>(Writer/OutputStream)` wrappers record the wrapped sink so
    // writes can be forwarded into the innermost buffer.
    if name == "<init>" {
        if let Some(&V::Ref(w)) = args.get(1) {
            pseudo_set(vm, this, P_DATA, V::Ref(w));
        }
        return Ok(Some(None));
    }
    // StringWriter/CharArrayWriter accumulate into a StrBuf — writes
    // append text and toString()/getBuffer() hand it back. A bare
    // `toString` that returned the identity string poisoned resource
    // strings in tasks.org (Okio.Utf8 getString via StringWriter).
    let wthis = wrapped_writer(vm, this).unwrap_or(this);
    let is_strbuf = matches!(
        vm.heap.get(wthis),
        Some(Obj::Host {
            state: HostState::StrBuf(_),
            ..
        })
    );
    if is_strbuf {
        match name {
            "write" => {
                let t = match args.get(1).copied() {
                    Some(V::I32(c)) => char::from_u32(c as u32)
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                    Some(V::Ref(ah)) => match vm.heap.get(ah) {
                        Some(Obj::Array { data, .. }) => {
                            // char[] is UTF-16 — decode code units so
                            // surrogate pairs (emoji) survive.
                            let lo = args.get(2).map(|v| i64v(*v).max(0) as usize).unwrap_or(0);
                            let hi = args
                                .get(3)
                                .map(|v| (lo + i64v(*v).max(0) as usize).min(data.len()))
                                .unwrap_or(data.len());
                            let units: Vec<u16> = data[lo.min(data.len())..hi]
                                .iter()
                                .map(|v| i64v(*v) as u16)
                                .collect();
                            String::from_utf16_lossy(&units)
                        }
                        _ => {
                            let t = sv(vm, args[1]).unwrap_or_default();
                            let lo = args.get(2).map(|v| i64v(*v).max(0) as usize).unwrap_or(0);
                            let hi = args
                                .get(3)
                                .map(|v| lo + i64v(*v).max(0) as usize)
                                .unwrap_or(t.chars().count());
                            t.chars().skip(lo).take(hi - lo.min(hi)).collect()
                        }
                    },
                    _ => String::new(),
                };
                if let Some(Obj::Host {
                    state: HostState::StrBuf(b),
                    ..
                }) = vm.heap.get_mut(wthis)
                {
                    b.push_str(&t);
                }
                return Ok(Some(None));
            }
            "append" => {
                let t = args
                    .get(1)
                    .map(|v| writer_text(vm, *v, args.get(2).copied(), args.get(3).copied(), false))
                    .unwrap_or_default();
                if let Some(Obj::Host {
                    state: HostState::StrBuf(b),
                    ..
                }) = vm.heap.get_mut(wthis)
                {
                    b.push_str(&t);
                }
                return Ok(Some(Some(args[0])));
            }
            "getBuffer" => {
                // StringWriter.getBuffer() returns the *shared* buffer:
                // hand back a StringBuffer view that delegates through
                // P_DATA to this writer's StrBuf so guest-side appends
                // land in the same storage.
                let b = host(vm, "Ljava/lang/StringBuffer;", HostState::Empty);
                if let V::Ref(bh) = b {
                    pseudo_set(vm, bh, P_DATA, V::Ref(wthis));
                }
                return Ok(Some(Some(b)));
            }
            "toString" | "toCharArray" => {
                let t = match vm.heap.get(wthis) {
                    Some(Obj::Host {
                        state: HostState::StrBuf(b),
                        ..
                    }) => b.clone(),
                    _ => String::new(),
                };
                if name == "toCharArray" {
                    // char[] is UTF-16 units, not scalar values.
                    let cs: Vec<V> = t.encode_utf16().map(|u| V::I32(u as i32)).collect();
                    return Ok(Some(Some(arr(vm, "C", cs))));
                }
                return Ok(Some(Some(jstr(vm, &t))));
            }
            "flush" | "close" => return Ok(Some(None)),
            _ => {}
        }
    }
    Ok(match name {
        "read" => {
            // Streams without a byte payload report EOF so callers exit
            // loops cleanly.
            Some(Some(V::I32(-1)))
        }
        "available" => Some(Some(V::I32(0))),
        "readLine" => {
            let found = if let Some(Obj::Host {
                state: HostState::Chars(chars, pos, _),
                ..
            }) = vm.heap.get_mut(this)
            {
                if *pos >= chars.len() {
                    None
                } else {
                    let start = *pos;
                    let mut end = start;
                    while end < chars.len() && chars[end] != 10 && chars[end] != 13 {
                        end += 1;
                    }
                    let line = String::from_utf16_lossy(&chars[start..end]);
                    if end < chars.len() {
                        let nl = chars[end];
                        end += 1;
                        if nl == 13 && end < chars.len() && chars[end] == 10 {
                            end += 1;
                        }
                    }
                    *pos = end;
                    Some(line)
                }
            } else {
                None
            };
            Some(Some(match found {
                Some(line) => jstr(vm, &line),
                None => V::Ref(0),
            }))
        }
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
// java.nio.file paths used by Okio/DataStore atomic writes.

fn path_text(vm: &Vm, v: V) -> Option<String> {
    fn scan(vm: &Vm, v: V, depth: u8) -> Option<String> {
        if depth > 4 {
            return None;
        }
        if let Some(p) = stream_arg_path(vm, v) {
            return Some(p);
        }
        let V::Ref(h) = v else { return None };
        let fields = match vm.heap.get(h) {
            Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                fields.values().copied().collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        // Okio Path wrappers are dex objects around a java.nio.file.Path
        // (or its string form). Prefer absolute-looking text, then any
        // nested path value.
        let mut relative = None;
        for f in fields {
            if let Some(p) = scan(vm, f, depth + 1) {
                if p.starts_with('/') {
                    return Some(p);
                }
                relative.get_or_insert(p);
            }
        }
        relative
    }
    scan(vm, v, 0)
}

fn file_millis(path: &std::path::Path) -> i64 {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn options_include(vm: &mut Vm, args: &[V], idx: usize, needle: &str) -> bool {
    let Some(V::Ref(h)) = args.get(idx) else {
        return false;
    };
    let values = match vm.heap.get(*h) {
        Some(Obj::Array { data, .. }) => data.clone(),
        _ => return false,
    };
    values
        .iter()
        .any(|v| v_to_s(vm, *v).to_ascii_uppercase().contains(needle))
}

fn nio_file_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    if class == "Ljava/nio/file/Paths;" && name == "get" {
        let mut out = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
        if let Some(V::Ref(more)) = args.get(1) {
            if let Some(Obj::Array { data, .. }) = vm.heap.get(*more) {
                for part in data {
                    if let Some(p) = sv(vm, *part) {
                        if !p.is_empty() {
                            if !out.is_empty() && !out.ends_with('/') {
                                out.push('/');
                            }
                            out.push_str(p.trim_start_matches('/'));
                        }
                    }
                }
            }
        }
        return Ok(Some(Some(host(
            vm,
            "Ljava/nio/file/Path;",
            HostState::Path(out),
        ))));
    }
    if class == "Ljava/nio/file/Path;" {
        let this = args.first().copied().unwrap_or(V::Ref(0));
        let path = path_text(vm, this).unwrap_or_default();
        return Ok(Some(match name {
            "resolve" => {
                let other = args.get(1).copied().unwrap_or(V::Ref(0));
                let tail = path_text(vm, other)
                    .or_else(|| sv(vm, other))
                    .unwrap_or_default();
                let joined = if tail.starts_with('/') || path.is_empty() {
                    tail
                } else {
                    format!("{}/{}", path.trim_end_matches('/'), tail)
                };
                Some(host(vm, class, HostState::Path(joined)))
            }
            "getParent" => {
                let p = std::path::Path::new(&path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string());
                Some(match p {
                    Some(p) => host(vm, class, HostState::Path(p)),
                    None => V::Ref(0),
                })
            }
            "getFileName" => {
                let n = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                Some(host(vm, class, HostState::Path(n)))
            }
            "toAbsolutePath" | "normalize" => Some(host(vm, class, HostState::Path(path))),
            "toFile" => Some(host(vm, "Ljava/io/File;", HostState::Path(path))),
            _ => return Ok(None),
        }));
    }
    if class == "Ljava/nio/file/Files;" {
        let path_at = |idx: usize| args.get(idx).copied().and_then(|v| path_text(vm, v));
        return Ok(Some(match name {
            "move" | "copy" => {
                let (Some(from), Some(to)) = (path_at(0), path_at(1)) else {
                    return Err(vm.throw_builtin("Ljava/io/IOException;", "missing path"));
                };
                let (from_h, to_h) = (fspath(vm, &from), fspath(vm, &to));
                if std::fs::symlink_metadata(&from_h).is_err() {
                    return Err(vm.throw_builtin("Ljava/nio/file/NoSuchFileException;", &from));
                }
                let replace = options_include(vm, args, 2, "REPLACE_EXISTING");
                if std::fs::symlink_metadata(&to_h).is_ok() && !replace {
                    return Err(vm.throw_builtin("Ljava/nio/file/FileAlreadyExistsException;", &to));
                }
                if let Some(parent) = to_h.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let ok = if name == "move" {
                    std::fs::rename(&from_h, &to_h).is_ok()
                        || (std::fs::copy(&from_h, &to_h).is_ok()
                            && std::fs::remove_file(&from_h).is_ok())
                } else {
                    std::fs::copy(&from_h, &to_h).is_ok()
                };
                if !ok {
                    return Err(vm.throw_builtin("Ljava/io/IOException;", "file operation failed"));
                }
                Some(args.get(1).copied().unwrap_or(V::Ref(0)))
            }
            "exists" | "notExists" | "isDirectory" | "isRegularFile" => {
                let p = path_at(0).map(|p| fspath(vm, &p)).unwrap_or_default();
                let yes = match name {
                    "exists" => p.exists(),
                    "notExists" => !p.exists(),
                    "isDirectory" => p.is_dir(),
                    _ => p.is_file(),
                };
                Some(V::I32(yes as i32))
            }
            "createDirectory" | "createDirectories" => {
                let p = path_at(0).map(|p| fspath(vm, &p)).unwrap_or_default();
                let _ = std::fs::create_dir_all(p);
                Some(args.first().copied().unwrap_or(V::Ref(0)))
            }
            "delete" | "deleteIfExists" => {
                let guest = path_at(0).unwrap_or_default();
                let p = fspath(vm, &guest);
                let Ok(meta) = std::fs::symlink_metadata(&p) else {
                    if name == "deleteIfExists" {
                        return Ok(Some(Some(V::I32(0))));
                    }
                    return Err(vm.throw_builtin("Ljava/nio/file/NoSuchFileException;", &guest));
                };
                let is_link = meta.file_type().is_symlink();
                let nonempty_dir = !is_link
                    && meta.is_dir()
                    && std::fs::read_dir(&p).is_ok_and(|mut d| d.next().is_some());
                if nonempty_dir {
                    return Err(
                        vm.throw_builtin("Ljava/nio/file/DirectoryNotEmptyException;", &guest)
                    );
                }
                let ok = if !is_link && meta.is_dir() {
                    std::fs::remove_dir(&p).is_ok()
                } else {
                    std::fs::remove_file(&p).is_ok()
                };
                if !ok {
                    return Err(vm.throw_builtin("Ljava/io/IOException;", "delete failed"));
                }
                Some(if name == "deleteIfExists" {
                    V::I32(1)
                } else {
                    V::I32(0)
                })
            }
            "newInputStream" => {
                let bytes = path_at(0)
                    .and_then(|p| std::fs::read(fspath(vm, &p)).ok())
                    .unwrap_or_default();
                Some(host(
                    vm,
                    "Ljava/io/FileInputStream;",
                    HostState::Bytes(std::rc::Rc::new(bytes), 0),
                ))
            }
            "newOutputStream" => {
                let file = path_at(0).and_then(|p| {
                    let p = fspath(vm, &p);
                    if let Some(parent) = p.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let append = options_include(vm, args, 1, "APPEND");
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(!append)
                        .append(append)
                        .open(p)
                        .ok()
                });
                let Some(f) = file else {
                    return Err(vm.throw_builtin("Ljava/io/IOException;", "open failed"));
                };
                Some(host(vm, "Ljava/io/FileOutputStream;", HostState::File(f)))
            }
            "readAttributes" => {
                let p = path_at(0).unwrap_or_default();
                Some(host(
                    vm,
                    "Ljava/nio/file/attribute/BasicFileAttributes;",
                    HostState::Path(p),
                ))
            }
            _ => return Ok(None),
        }));
    }
    if class == "Ljava/nio/file/attribute/BasicFileAttributes;" {
        let path = path_text(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
        let p = fspath(vm, &path);
        let ms = file_millis(&p);
        return Ok(Some(Some(match name {
            "size" => V::I64(p.metadata().map(|m| m.len() as i64).unwrap_or(0)),
            "isDirectory" => V::I32(p.is_dir() as i32),
            "isRegularFile" => V::I32(p.is_file() as i32),
            "isSymbolicLink" => V::I32(
                p.symlink_metadata()
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false) as i32,
            ),
            "lastModifiedTime" | "lastAccessTime" | "creationTime" => host(
                vm,
                "Ljava/nio/file/attribute/FileTime;",
                HostState::Millis(ms),
            ),
            _ => return Ok(None),
        })));
    }
    if class == "Ljava/nio/file/attribute/FileTime;" && name == "toMillis" {
        let ms = match args.first() {
            Some(V::Ref(h)) => match vm.heap.get(*h) {
                Some(Obj::Host {
                    state: HostState::Millis(ms),
                    ..
                }) => *ms,
                _ => 0,
            },
            _ => 0,
        };
        return Ok(Some(Some(V::I64(ms))));
    }
    Ok(None)
}

// ---------------------------------------------------------------------
// File

/// Guest-visible path -> host path under `vm.guest_root`. Absolute
/// guest paths and `..` can never escape the sandbox: root/prefix
/// components are dropped and `..` above the root is discarded.
fn abs_guest_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn canon_guest_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let abs = abs_guest_path(path);
    for part in abs.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    format!("/{}", out.join("/"))
}

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
            let p = abs_guest_path(&path);
            let p = if name == "getCanonicalPath" {
                canon_guest_path(&p)
            } else {
                p
            };
            Some(jstr(vm, &p))
        }
        "getAbsoluteFile" | "getCanonicalFile" => {
            let p = abs_guest_path(&path);
            let p = if name == "getCanonicalFile" {
                canon_guest_path(&p)
            } else {
                p
            };
            Some(host(vm, "Ljava/io/File;", HostState::Path(p)))
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
            let to = sv(vm, args.get(1).copied().unwrap_or(V::Ref(0))).unwrap_or_default();
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
            if crate::dbg_flag("OVELO_VIEWDBG") {
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
        "createTempFile" => {
            // Static method: args are (prefix, suffix, directory?) with no
            // receiver. Android apps rely on unique, real files here; a
            // shared placeholder makes downloads write outside the cache or
            // overwrite one another.
            let prefix = sv(vm, args[0]).unwrap_or_else(|| "tmp".to_string());
            let suffix = sv(vm, args[1]).unwrap_or_else(|| ".tmp".to_string());
            let dir = args
                .get(2)
                .and_then(|v| stream_arg_path(vm, *v))
                .unwrap_or_else(|| "/data/local/ovelo/cache".to_string());
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = format!("{}-{}-{}{}", prefix, std::process::id(), n, suffix);
            let safe = name.replace('/', "_");
            let guest = format!("{dir}/{safe}");
            let host_path = fspath(vm, &guest);
            if let Some(parent) = host_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(host_path);
            Some(host(vm, "Ljava/io/File;", HostState::Path(guest)))
        }
        "setReadable" | "setWritable" | "setExecutable" | "setReadOnly" | "setLastModified"
        | "deleteOnExit" | "toPath" | "toURI" | "toURL" | "listRoots" | "compareTo" => {
            if name == "listRoots" {
                Some(host(vm, "Ljava/io/File;", HostState::Path("/".into())))
            } else if name == "toPath" {
                Some(host(vm, "Ljava/nio/file/Path;", HostState::Path(path)))
            } else if matches!(name, "toURI" | "toURL") {
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
                        (!t.is_empty() && class_ok(spec, t[0]) && m(rest, &t[1..])) || m(rest, t)
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
            Some(Some(V::I32(
                (full_match(&pat, &t) || literal_match(&pat, &t)) as i32,
            )))
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
            let lim = args.get(2).map(|v| i32v(*v)).unwrap_or(0);
            let parts: Vec<V> = rx::regex_split(&pat, &t, lim)
                .into_iter()
                .map(|x| jstr(vm, &x))
                .collect();
            Some(Some(arr(vm, STRING, parts)))
        }
        "quote" => Some(args.first().copied()),
        _ => None,
    }
}

// Matcher state pseudo-fields (`P_*` sentinels live in mod.rs): the
// last match's group spans, the match region, and find()'s resume point.
const P_RX_SPANS: u64 = 0xFFFD_FEF4;
const P_RX_LO: u64 = 0xFFFD_FEF5;
const P_RX_HI: u64 = 0xFFFD_FEF6;
const P_RX_FROM: u64 = 0xFFFD_FEF7;

/// Persist a search result on the matcher: `Ref(0)` = searched, no
/// match; `Ref([I)` = flat slot array `[g0s, g0e, g1s, g1e, …]`, -1 for
/// a group that did not participate.
fn rx_store(vm: &mut Vm, h: u32, m: Option<Vec<Option<usize>>>) {
    match m {
        None => pseudo_set(vm, h, P_RX_SPANS, V::Ref(0)),
        Some(s) => {
            let flat: Vec<V> = s
                .iter()
                .map(|o| V::I32(o.map(|x| x as i32).unwrap_or(-1)))
                .collect();
            let a = arr(vm, "I", flat);
            pseudo_set(vm, h, P_RX_SPANS, a);
        }
    }
}

/// The stored span array, or None when no search ran yet.
fn rx_spans(vm: &Vm, h: u32) -> Option<Vec<i32>> {
    match pseudo_get(vm, h, P_RX_SPANS)? {
        V::Ref(0) => None,
        V::Ref(a) => match vm.heap.get(a) {
            Some(Obj::Array { data, .. }) => Some(data.iter().map(|v| i32v(*v)).collect()),
            _ => None,
        },
        _ => None,
    }
}

fn matcher_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let text = slot_of(vm, h).and_then(|v| sv(vm, v)).unwrap_or_default();
    let t: Vec<char> = text.chars().collect();
    let pat = pseudo_get(vm, h, P_ACTION)
        .and_then(|v| sv(vm, v))
        .unwrap_or_default();
    let lo = pseudo_get(vm, h, P_RX_LO)
        .map(|v| i32v(v).max(0) as usize)
        .unwrap_or(0);
    let hi = pseudo_get(vm, h, P_RX_HI)
        .map(|v| i32v(v).max(0) as usize)
        .unwrap_or(t.len());
    let rx = || Rx::compile(&pat).ok();
    Ok(match name {
        "matches" => {
            let m = rx().and_then(|rx| rx.matches(&t, lo, hi));
            let found = m.is_some() || literal_match(&pat, &text);
            rx_store(vm, h, m);
            Some(Some(V::I32(found as i32)))
        }
        "lookingAt" => {
            let m = rx().and_then(|rx| rx.looking_at(&t, lo, lo, hi));
            let found = m.is_some() || literal_match(&pat, &text);
            rx_store(vm, h, m);
            Some(Some(V::I32(found as i32)))
        }
        "find" => {
            let from = if args.len() > 1 {
                i32v(args[1]).max(0) as usize
            } else {
                pseudo_get(vm, h, P_RX_FROM)
                    .map(|v| i32v(v).max(0) as usize)
                    .unwrap_or(lo)
            };
            let m = rx().and_then(|rx| rx.find(&t, lo, from, hi));
            let found = m.is_some() || (!pat.is_empty() && text.contains(&pat));
            if let Some(s) = &m {
                let st = s[0].unwrap_or(0) as i32;
                let en = s[1].unwrap_or(0) as i32;
                // Java resumes after the match; empty matches advance 1.
                pseudo_set(vm, h, P_RX_FROM, V::I32(if en > st { en } else { en + 1 }));
            }
            rx_store(vm, h, m);
            Some(Some(V::I32(found as i32)))
        }
        "group" => {
            let i = i32v(*args.get(1).unwrap_or(&V::I32(0))).max(0) as usize;
            let chars: Vec<char> = text.chars().collect();
            let out = match rx_spans(vm, h) {
                Some(sp) if 2 * i + 1 < sp.len() && sp[2 * i] >= 0 => {
                    let (a, b) = (sp[2 * i] as usize, sp[2 * i + 1] as usize);
                    jstr(
                        vm,
                        &chars[a.min(chars.len())..b.min(chars.len())]
                            .iter()
                            .collect::<String>(),
                    )
                }
                _ => V::Ref(0),
            };
            Some(Some(out))
        }
        "groupCount" => {
            let n = rx().map(|rx| rx.ngroups()).unwrap_or(0) as i32;
            Some(Some(V::I32(n)))
        }
        "start" | "end" => {
            let i = i32v(*args.get(1).unwrap_or(&V::I32(0))).max(0) as usize;
            let v = match rx_spans(vm, h) {
                Some(sp) if 2 * i + 1 < sp.len() => {
                    sp[if name == "start" { 2 * i } else { 2 * i + 1 }]
                }
                _ => -1,
            };
            Some(Some(V::I32(v)))
        }
        "region" => {
            let a = i32v(*args.get(1).unwrap_or(&V::I32(0))).max(0);
            let b = i32v(*args.get(2).unwrap_or(&V::I32(0))).max(0);
            pseudo_set(vm, h, P_RX_LO, V::I32(a));
            pseudo_set(vm, h, P_RX_HI, V::I32(b));
            Some(Some(args[0]))
        }
        "regionStart" => Some(Some(V::I32(lo as i32))),
        "regionEnd" => Some(Some(V::I32(hi as i32))),
        "replaceAll" | "replaceFirst" => {
            let rep = sv(vm, args[1]).unwrap_or_default();
            let out = if let Some(rx) = rx() {
                rx_replace(&rx, &t, &rep, name == "replaceFirst")
            } else if pat.is_empty() {
                text.clone()
            } else if name == "replaceFirst" {
                text.replacen(&pat, &rep, 1)
            } else {
                text.replace(&pat, &rep)
            };
            Some(Some(jstr(vm, &out)))
        }
        "quoteReplacement" => Some(args.first().copied()),
        "reset" => Some(Some(args[0])),
        "usePattern" => {
            if let Some(p2) = args.get(1).and_then(|v| sv(vm, *v)) {
                let pv = jstr(vm, &p2);
                pseudo_set(vm, h, P_ACTION, pv);
            }
            Some(Some(args[0]))
        }
        "useAnchoringBounds" | "useTransparentBounds" | "appendReplacement" => Some(Some(args[0])),
        "appendTail" => Some(Some(args[1])),
        "pattern" => Some(Some(args[0])),
        "requireEnd" | "hasAnchoringBounds" | "hasTransparentBounds" => Some(Some(V::I32(0))),
        "namedGroups" => Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![])))),
        _ => None,
    })
}

/// `Matcher.replaceAll/First` over real match spans, expanding `$g` and
/// `${g}` references in the replacement (Java semantics minus quoting —
/// callers pass `quoteReplacement` output through already).
pub(crate) fn rx_replace(rx: &Rx, t: &[char], rep: &str, first_only: bool) -> String {
    let mut out: Vec<char> = Vec::new();
    let rep_chars: Vec<char> = rep.chars().collect();
    let mut pos = 0usize;
    let hi = t.len();
    while let Some(m) = rx.find(t, 0, pos, hi) {
        let (ms, me) = (m[0].unwrap_or(pos), m[1].unwrap_or(pos));
        out.extend_from_slice(&t[pos..ms]);
        // expand $d / ${name-free digits}
        let mut i = 0;
        while i < rep_chars.len() {
            if rep_chars[i] == '$' && i + 1 < rep_chars.len() {
                if rep_chars[i + 1] == '{' {
                    if let Some(close) = rep_chars[i + 2..].iter().position(|c| *c == '}') {
                        let name: String = rep_chars[i + 2..i + 2 + close].iter().collect();
                        if let Ok(g) = name.parse::<usize>() {
                            if 2 * g + 1 < m.len() {
                                if let (Some(a), Some(b)) = (m[2 * g], m[2 * g + 1]) {
                                    out.extend_from_slice(&t[a..b]);
                                }
                            }
                        }
                        i += 3 + close;
                        continue;
                    }
                } else if rep_chars[i + 1].is_ascii_digit() {
                    let mut j = i + 1;
                    while j < rep_chars.len() && rep_chars[j].is_ascii_digit() {
                        j += 1;
                    }
                    // Longest valid group number wins ($12 → g12 else g1 + '2').
                    let mut k = j;
                    loop {
                        let g: usize = rep_chars[i + 1..k]
                            .iter()
                            .collect::<String>()
                            .parse()
                            .unwrap_or(usize::MAX);
                        if 2 * g + 1 < m.len() || k <= i + 2 {
                            if 2 * g + 1 < m.len() {
                                if let (Some(a), Some(b)) = (m[2 * g], m[2 * g + 1]) {
                                    out.extend_from_slice(&t[a..b]);
                                }
                            }
                            out.extend_from_slice(&rep_chars[k..j]);
                            break;
                        }
                        k -= 1;
                    }
                    i = j;
                    continue;
                }
            }
            out.push(rep_chars[i]);
            i += 1;
        }
        if first_only {
            pos = me;
            break;
        }
        pos = if me > ms {
            me
        } else {
            // Zero-width match: the char it sits on is unmatched text —
            // emit it before advancing past it (Java emits it through
            // the following `t[pos..ms]` region).
            if me < hi {
                out.push(t[me]);
            }
            me + 1
        };
        if pos > hi {
            pos = hi;
            break;
        }
    }
    if pos <= hi {
        out.extend_from_slice(&t[pos.min(hi)..]);
    }
    out.into_iter().collect()
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
        // flags/width/precision, ending at the conversion letter itself
        let mut spec = String::new();
        let conv = loop {
            let Some(c2) = chars.next() else {
                break 's';
            };
            if c2.is_ascii_alphabetic() {
                break c2;
            }
            spec.push(c2);
        };
        let v = args.get(i).copied().unwrap_or(V::Ref(0));
        i += 1;
        let body = match conv {
            'd' | 'x' | 'X' | 'o' => {
                let n = i64v(match v {
                    V::Ref(h) => slot_of(vm, h).unwrap_or(v),
                    _ => v,
                });
                let width = spec
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<usize>()
                    .unwrap_or(0);
                // Java formats negative Byte/Short/Integer/Long values as
                // unsigned at their type width. Narrow host wrappers carry
                // their type in the descriptor; primitive slots default to
                // the platform integer width.
                let bits = match v {
                    V::Ref(h) => match vm.heap.get(h) {
                        Some(Obj::Host { desc, .. }) => match desc.as_str() {
                            "Ljava/lang/Byte;" => 8,
                            "Ljava/lang/Short;" => 16,
                            "Ljava/lang/Integer;" => 32,
                            _ => 64,
                        },
                        _ => 64,
                    },
                    _ => 32,
                };
                let mask = if bits == 64 {
                    u64::MAX
                } else {
                    (1u64 << bits) - 1
                };
                let u = (n as u64) & mask;
                match conv {
                    'x' => format!("{u:0width$x}"),
                    'X' => format!("{u:0width$X}"),
                    'o' => format!("{u:0width$o}"),
                    _ => format!("{n:0width$}"),
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
                // Box booleans as java.lang.Boolean mirrors — a bare
                // I32 stored into an Object[] canonicalizes to null
                // and dumps as 0/1 instead of false/true.
                "true" => Some(host(vm, "Ljava/lang/Boolean;", HostState::Slot(V::I32(1)))),
                "false" => Some(host(vm, "Ljava/lang/Boolean;", HostState::Slot(V::I32(0)))),
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

/// A boxed primitive stored in a JSON container reads back as the
/// wrapped value for the numeric getters (`getBoolean`/`optInt`/…).
fn json_unbox(vm: &Vm, v: V) -> V {
    match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::Slot(inner),
                ..
            }) => *inner,
            _ => v,
        },
        x => x,
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_dump_v(vm: &Vm, v: V) -> String {
    json_dump_d(vm, v, 0)
}

fn json_dump_d(vm: &Vm, v: V, depth: u32) -> String {
    // Depth cap: a cyclic structure built with put() would otherwise
    // recurse until the host stack dies — real org.json throws
    // StackOverflowError; degrading the cycle to null keeps the host up.
    if depth > 64 {
        return "null".into();
    }
    match v {
        V::Ref(0) => "null".into(),
        V::Ref(h) => {
            // Mirrored java.lang.String instances are not Obj::Str, but
            // they are still JSON string values.
            let as_string = match vm.heap.get(h) {
                Some(Obj::Str(s)) => Some(s.clone()),
                Some(Obj::Host { desc, .. }) if desc == "Ljava/lang/String;" => sv(vm, V::Ref(h)),
                _ => None,
            };
            if let Some(text) = as_string {
                return format!("\"{}\"", json_escape(&text));
            }
            match vm.heap.get(h) {
                Some(Obj::Host {
                    desc,
                    state: HostState::Slot(v),
                    ..
                }) if desc == "Ljava/lang/Boolean;" => {
                    if matches!(v, V::I32(0)) {
                        "false".into()
                    } else {
                        "true".into()
                    }
                }
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => format!(
                    "{{{}}}",
                    m.iter()
                        .map(|(k, v)| {
                            let ks = sv(vm, *k).unwrap_or_default();
                            format!(
                                "\"{}\":{}",
                                json_escape(&ks),
                                json_dump_d(vm, *v, depth + 1)
                            )
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
                        .map(|v| json_dump_d(vm, *v, depth + 1))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                _ => "null".into(),
            }
        }
        x => fmt_v(x),
    }
}

fn json_coerce_number(vm: &Vm, v: V, name: &str) -> V {
    let parsed = sv(vm, v);
    let string_num = parsed.as_deref().and_then(|s| s.parse::<f64>().ok());
    if name.ends_with("Int") {
        let out = match v {
            V::I32(i) => i,
            // Long.intValue is a narrowing integer conversion.
            V::I64(i) => i as i32,
            // Floating-point intValue clamps out-of-range values.
            V::F32(f) => f as i32,
            V::F64(f) => f as i32,
            _ => string_num.map(|f| f as i32).unwrap_or(0),
        };
        V::I32(out)
    } else if name.ends_with("Long") {
        let out = match v {
            V::I32(i) => i as i64,
            V::I64(i) => i,
            V::F32(f) => f as i64,
            V::F64(f) => f as i64,
            _ => string_num.map(|f| f as i64).unwrap_or(0),
        };
        V::I64(out)
    } else if name.ends_with("Double") {
        V::F64(string_num.unwrap_or_else(|| f64v(v)))
    } else {
        let truthy = parsed
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|| i64v(v) != 0);
        V::I32(truthy as i32)
    }
}

fn json_obj(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let h = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => return Ok(None),
    };
    Ok(match name {
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
        "get" | "opt" => match map_get(vm, h, args[1]) {
            Some(V::Ref(0)) => Some(json_null(vm)),
            v => v.or(Some(V::Ref(0))),
        },
        "getJSONObject" | "getJSONArray" | "optJSONObject" | "optJSONArray" => {
            let v = match map_get(vm, h, args[1]) {
                Some(V::Ref(0)) => json_null(vm),
                Some(v) => v,
                None => V::Ref(0),
            };
            let want_arr = name.ends_with("JSONArray");
            let is_null = matches!(v, V::Ref(0)) || matches!(v, V::Ref(vh) if vh == vm.json_null_h);
            let matches_type = matches!(v, V::Ref(vh) if vh != 0 && matches!(
                vm.heap.get(vh),
                Some(Obj::Host { desc, .. })
                    if (want_arr && desc == "Lorg/json/JSONArray;")
                        || (!want_arr && desc == "Lorg/json/JSONObject;")
            ));
            if matches_type {
                Some(v)
            } else if name.starts_with("opt") {
                Some(V::Ref(0))
            } else {
                let key = sv(vm, args[1]).unwrap_or_default();
                let why = if is_null {
                    format!("Value null at {key} is not an object")
                } else {
                    format!("Value at {key} has the wrong type")
                };
                return Err(vm.throw_builtin("Lorg/json/JSONException;", &why));
            }
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
                Some(v) if !matches!(v, V::Ref(0)) => {
                    Some(json_coerce_number(vm, json_unbox(vm, v), name))
                }
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
        _ => return Ok(None),
    }
    .map(Some))
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
                    .map(|v| json_unbox(vm, v))
                    .unwrap_or_else(|| args.get(2).copied().unwrap_or(V::I32(0))),
            )
        }
        "length" => Some(V::I32(list_ref(vm, h).map(|l| l.len() as i32).unwrap_or(0))),
        // Dex code treats JSON arrays as java.util.List when a parsed
        // map is consumed generically (`asFloatList(list.get(k))`) —
        // answer the List shape too so interface dispatch resolves.
        "size" => Some(V::I32(list_ref(vm, h).map(|l| l.len() as i32).unwrap_or(0))),
        "isEmpty" => Some(V::I32(
            list_ref(vm, h).map(|l| l.is_empty()).unwrap_or(true) as i32,
        )),
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

/// Split an absolute URL text into its JDK `URL` components. Missing
/// parts come back empty/`-1`, matching the platform's lenient parse
/// (no validation — `new URL` in the guest accepts anything).
fn url_parts(u: &str) -> (String, String, i32, String, String, String) {
    // scheme — text before "://" or, for opaque URIs, before ':'
    let (scheme, rest) = match u.find("://") {
        Some(i) => (&u[..i], &u[i + 3..]),
        None => match u.find(':') {
            Some(i) if i > 0 => (&u[..i], &u[i + 1..]),
            _ => ("", u),
        },
    };
    // authority ends at the first / ? #
    let aend = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..aend];
    let after = &rest[aend..];
    let (_userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (host_s, port) = match hostport.rfind(':') {
        Some(i) if hostport[i + 1..].parse::<i32>().is_ok() => (
            &hostport[..i],
            hostport[i + 1..].parse::<i32>().unwrap_or(-1),
        ),
        _ => (hostport, -1),
    };
    // path / query / ref
    let (no_ref, ref_s) = match after.find('#') {
        Some(i) => (&after[..i], &after[i + 1..]),
        None => (after, ""),
    };
    let (path, query) = match no_ref.find('?') {
        Some(i) => (&no_ref[..i], &no_ref[i + 1..]),
        None => (no_ref, ""),
    };
    (
        scheme.to_string(),
        host_s.to_string(),
        port,
        path.to_string(),
        query.to_string(),
        ref_s.to_string(),
    )
}

fn url_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let t = s(
        vm,
        match args.first() {
            Some(V::Ref(h)) => *h,
            _ => 0,
        },
    )
    .unwrap_or_default();
    Ok(match name {
        "<init>" => {
            // URL(String) / URL(URL, String) — keep the spec text so the
            // component getters can parse it. A base URL resolves a
            // relative spec by joining at the last '/' of its path.
            let (spec, base) = if args.len() >= 3 {
                (sv(vm, args[2]).unwrap_or_default(), t.clone())
            } else {
                (
                    sv(vm, args.get(1).copied().unwrap_or(V::Ref(0))).unwrap_or_default(),
                    String::new(),
                )
            };
            let resolved = if spec.contains("://") || spec.starts_with('/') || base.is_empty() {
                spec
            } else {
                let trim = match base.find(['?', '#']) {
                    Some(i) => &base[..i],
                    None => &base[..],
                };
                match trim.rfind('/') {
                    Some(i) => format!("{}{spec}", &trim[..i + 1]),
                    None => spec,
                }
            };
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(*h) {
                    *state = HostState::StrBuf(resolved);
                }
            }
            Some(None)
        }
        "openConnection" | "openStream" | "getContent" => Some(Some(host(
            vm,
            "Ljava/net/URLConnection;",
            HostState::StrBuf(t),
        ))),
        "toURI" => Some(Some(host(vm, "Ljava/net/URI;", HostState::StrBuf(t)))),
        "toURL" => Some(Some(host(vm, "Ljava/net/URL;", HostState::StrBuf(t)))),
        "getProtocol" => {
            let (scheme, ..) = url_parts(&t);
            Some(Some(jstr(vm, &scheme)))
        }
        "getHost" => {
            let (_, host_s, ..) = url_parts(&t);
            Some(Some(jstr(vm, &host_s)))
        }
        "getPort" => {
            let (_, _, port, ..) = url_parts(&t);
            Some(Some(V::I32(port)))
        }
        "getDefaultPort" => {
            let p = match t.split("://").next().unwrap_or("") {
                "https" | "wss" => 443,
                "http" | "ws" | "ftp" => 80,
                _ => -1,
            };
            Some(Some(V::I32(p)))
        }
        "getAuthority" => {
            let authority = if let Some(i) = t.find("://") {
                let rest = &t[i + 3..];
                let e = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                rest[..e].to_string()
            } else {
                String::new()
            };
            Some(Some(jstr(vm, &authority)))
        }
        "getUserInfo" => {
            let ui = if let Some(i) = t.find("://") {
                let rest = &t[i + 3..];
                let e = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                match rest[..e].rfind('@') {
                    Some(j) => rest[..j].to_string(),
                    None => String::new(),
                }
            } else {
                String::new()
            };
            Some(Some(jstr(vm, &ui)))
        }
        "getPath" => {
            let (_, _, _, path, ..) = url_parts(&t);
            Some(Some(jstr(vm, &path)))
        }
        "getQuery" => {
            let (_, _, _, _, query, _) = url_parts(&t);
            Some(Some(jstr(vm, &query)))
        }
        "getRef" => {
            let (_, _, _, _, _, ref_s) = url_parts(&t);
            Some(Some(jstr(vm, &ref_s)))
        }
        "getFile" => {
            let (_, _, _, path, query, _) = url_parts(&t);
            let f = if query.is_empty() {
                path
            } else {
                format!("{path}?{query}")
            };
            Some(Some(jstr(vm, &f)))
        }
        "toExternalForm" | "toString" => Some(Some(jstr(vm, &t))),
        "sameFile" | "equals" => {
            let other = s(
                vm,
                match args.get(1) {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                },
            )
            .unwrap_or_default();
            Some(Some(V::I32((t == other) as i32)))
        }
        "hashCode" => {
            Some(Some(V::I32(t.bytes().fold(0i32, |a, b| {
                a.wrapping_mul(31).wrapping_add(b as i32)
            }))))
        }
        _ => None,
    })
}

/// Normalized charset name ("UTF-8", "ISO-8859-1", "US-ASCII").
fn charset_name(vm: &Vm, h: u32) -> String {
    s(vm, h)
        .unwrap_or_else(|| "UTF-8".into())
        .to_uppercase()
        .replace(['-', '_'], "")
}

/// Remaining bytes of a `ByteBuffer`-like host buffer (Slot-backed
/// array at base+position), with the position the bytes start at.
fn bb_bytes(vm: &Vm, h: u32) -> (Vec<u8>, usize) {
    let (back, base, pos, lim, es) = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Slot(V::Ref(b)),
            ..
        }) => {
            let base = i32v(pseudo_get(vm, h, P_BBUF_BASE).unwrap_or(V::I32(0))).max(0) as usize;
            let pos = i32v(pseudo_get(vm, h, P_BBUF_POS).unwrap_or(V::I32(0))).max(0) as usize;
            let es = i32v(pseudo_get(vm, h, P_BBUF_ESZ).unwrap_or(V::I32(1))).clamp(1, 8) as usize;
            let lim = pseudo_get(vm, h, P_BBUF_LIM)
                .map(|v| i32v(v).max(0) as usize)
                .unwrap_or(usize::MAX);
            (*b, base, pos, lim, es)
        }
        Some(Obj::Array { .. }) => (h, 0, 0, usize::MAX, 1),
        _ => return (Vec::new(), 0),
    };
    match vm.heap.get(back) {
        Some(Obj::Array { data, .. }) => (
            data.iter()
                .skip(base + pos * es)
                .take(lim.saturating_sub(pos) * es)
                .map(|v| i32v(*v) as u8)
                .collect(),
            pos,
        ),
        _ => (Vec::new(), 0),
    }
}

/// Remaining chars of a `CharBuffer`-like host (Slot-backed `C` array
/// at base+position). Other buffer kinds return None.
fn charbuf_string(vm: &Vm, h: u32) -> Option<String> {
    let (back, base, pos, lim) = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Slot(V::Ref(b)),
            ..
        }) => (
            *b,
            i32v(pseudo_get(vm, h, P_BBUF_BASE).unwrap_or(V::I32(0))).max(0) as usize,
            i32v(pseudo_get(vm, h, P_BBUF_POS).unwrap_or(V::I32(0))).max(0) as usize,
            pseudo_get(vm, h, P_BBUF_LIM)
                .map(|v| i32v(v).max(0) as usize)
                .unwrap_or(usize::MAX),
        ),
        Some(Obj::Array { .. }) => (h, 0, 0, usize::MAX),
        _ => return None,
    };
    match vm.heap.get(back) {
        Some(Obj::Array { data, .. }) => {
            let mut units = Vec::new();
            for v in data.iter().skip(base + pos).take(lim.saturating_sub(pos)) {
                let x = i32v(*v) as u32;
                if x <= 0xffff {
                    units.push(x as u16);
                } else if let Some(ch) = char::from_u32(x) {
                    let mut pair = [0u16; 2];
                    units.extend_from_slice(ch.encode_utf16(&mut pair));
                } else {
                    units.push(0xfffd);
                }
            }
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// Decode `bytes` under `cs`; when `eoi` is false an incomplete UTF-8
/// tail stays unconsumed — returns (text, consumed_byte_count).
fn cs_decode(cs: &str, bytes: &[u8], eoi: bool) -> (String, usize) {
    if cs.contains("88591") || cs.contains("LATIN1") {
        return (bytes.iter().map(|b| *b as char).collect(), bytes.len());
    }
    if cs.contains("ASCII") {
        return (
            bytes
                .iter()
                .map(|b| if *b < 0x80 { *b as char } else { '\u{FFFD}' })
                .collect(),
            bytes.len(),
        );
    }
    // UTF-8 (default): at end-of-input, lossy; mid-stream, cut an
    // incomplete trailing sequence and leave it for the next chunk.
    match std::str::from_utf8(bytes) {
        Ok(t) => (t.to_string(), bytes.len()),
        Err(e) => {
            let valid = e.valid_up_to();
            if eoi || e.error_len().is_some() {
                (String::from_utf8_lossy(bytes).into_owned(), bytes.len())
            } else {
                (String::from_utf8_lossy(&bytes[..valid]).into_owned(), valid)
            }
        }
    }
}

/// `decode(ByteBuffer)` → CharBuffer of the decoded content, and
/// `decode(in, out, eoi)` — chars appended into `out`'s backing.
fn charset_decode(vm: &mut Vm, args: &[V]) -> V {
    let cs = args
        .first()
        .and_then(|v| match v {
            V::Ref(h) => Some(charset_name(vm, *h)),
            _ => None,
        })
        .unwrap_or_else(|| "UTF8".into());
    let in_h = match args.get(1) {
        Some(&V::Ref(h)) => h,
        _ => return V::Ref(0),
    };
    let (bytes, in_pos) = bb_bytes(vm, in_h);
    let eoi = args.get(3).map(|v| i32v(*v) != 0).unwrap_or(true);
    let (text, consumed) = cs_decode(&cs, &bytes, eoi);
    if let Some(&V::Ref(out_h)) = args.get(2) {
        // (ByteBuffer, CharBuffer, boolean): append chars to `out`.
        let (back, base, pos, cap) = match vm.heap.get(out_h) {
            Some(Obj::Host {
                state: HostState::Slot(V::Ref(b)),
                ..
            }) => {
                let base =
                    i32v(pseudo_get(vm, out_h, P_BBUF_BASE).unwrap_or(V::I32(0))).max(0) as usize;
                let pos =
                    i32v(pseudo_get(vm, out_h, P_BBUF_POS).unwrap_or(V::I32(0))).max(0) as usize;
                let lim = pseudo_get(vm, out_h, P_BBUF_LIM)
                    .map(|v| i32v(v).max(0) as usize)
                    .unwrap_or(usize::MAX);
                let cap = match vm.heap.get(*b) {
                    // Limits and positions are buffer-relative. In particular a
                    // sliced buffer has both a nonzero backing base and a
                    // limit relative to the slice; subtracting the base here
                    // would shrink (or empty) the writable window twice.
                    Some(Obj::Array { data, .. }) => lim
                        .saturating_sub(pos)
                        .min(data.len().saturating_sub(base + pos)),
                    _ => 0,
                };
                (*b, base, pos, cap)
            }
            _ => (0, 0, 0, 0),
        };
        // Java CharBuffer storage is UTF-16. Supplementary scalars take
        // two units and must never be split by an output-buffer boundary.
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut wrote = units.len().min(cap);
        if wrote < units.len() && wrote > 0 && (0xD800..0xDC00).contains(&units[wrote - 1]) {
            wrote -= 1;
        }
        if back != 0 {
            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(back) {
                for (i, unit) in units.iter().take(wrote).enumerate() {
                    if let Some(slot) = data.get_mut(base + pos + i) {
                        *slot = V::I32(*unit as i32);
                    }
                }
            }
        }
        pseudo_set(vm, out_h, P_BBUF_POS, V::I32((pos + wrote) as i32));
        // Only the bytes backing wholly written characters are consumed —
        // OVERFLOW leaves the rest for the next decode call.
        let used = if wrote < units.len() {
            let single_byte = cs.contains("88591") || cs.contains("LATIN1") || cs.contains("ASCII");
            let mut used_units = 0usize;
            let mut used_bytes = 0usize;
            for ch in text.chars() {
                let char_units = ch.len_utf16();
                if used_units + char_units > wrote {
                    break;
                }
                used_units += char_units;
                used_bytes += if single_byte { 1 } else { ch.len_utf8() };
            }
            used_bytes
        } else {
            consumed
        };
        pseudo_set(vm, in_h, P_BBUF_POS, V::I32((in_pos + used) as i32));
        let st = if wrote < units.len() {
            "OVERFLOW"
        } else {
            "UNDERFLOW"
        };
        return host(
            vm,
            "Ljava/nio/charset/CoderResult;",
            HostState::StrBuf(st.into()),
        );
    }
    // (ByteBuffer) → CharBuffer
    pseudo_set(vm, in_h, P_BBUF_POS, V::I32((in_pos + consumed) as i32));
    let arr = vm.heap.alloc(Obj::Array {
        elem: "C".into(),
        data: text.chars().map(|c| V::I32(c as i32)).collect(),
    });
    host(vm, "Ljava/nio/CharBuffer;", HostState::Slot(V::Ref(arr)))
}

fn charset_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    if class == "Ljava/nio/charset/CoderResult;" {
        let st = match args.first() {
            Some(&V::Ref(h)) => s(vm, h).unwrap_or_else(|| "UNDERFLOW".into()),
            _ => "UNDERFLOW".into(),
        };
        return match name {
            "isUnderflow" => Some(Some(V::I32((st == "UNDERFLOW") as i32))),
            "isOverflow" => Some(Some(V::I32((st == "OVERFLOW") as i32))),
            "isError" | "isMalformed" | "isUnmappable" => Some(Some(V::I32(0))),
            "length" => Some(Some(V::I32(0))),
            "throwException" | "toString" => {
                if name == "toString" {
                    Some(Some(jstr(vm, &st)))
                } else {
                    Some(None)
                }
            }
            _ => None,
        };
    }
    if class == "Ljava/nio/charset/CodingErrorAction;" {
        return match name {
            "toString" | "name" => Some(Some(jstr(vm, "REPLACE"))),
            _ => None,
        };
    }
    if class == "Ljava/nio/charset/CharsetDecoder;" || class == "Ljava/nio/charset/CharsetEncoder;"
    {
        return match name {
            "onMalformedInput" | "onUnmappableCharacter" | "replaceWith" | "reset" => {
                Some(Some(args[0]))
            }
            "malformedInputAction" | "unmappableCharacterAction" => Some(Some(host(
                vm,
                "Ljava/nio/charset/CodingErrorAction;",
                HostState::StrBuf("REPLACE".into()),
            ))),
            "replacement" => {
                // Encoder's replacement is a byte[] — the JDK default
                // is the single byte '?' — decoder's a String.
                if class == "Ljava/nio/charset/CharsetEncoder;" {
                    Some(Some(arr(vm, "B", vec![V::I32(b'?' as i32)])))
                } else {
                    Some(Some(jstr(vm, "\u{FFFD}")))
                }
            }
            "charset" | "detectedCharset" => Some(Some(host(
                vm,
                "Ljava/nio/charset/Charset;",
                HostState::StrBuf("UTF-8".into()),
            ))),
            "averageCharsPerByte" | "averageBytesPerChar" => Some(Some(V::F32(1.0))),
            "maxCharsPerByte" | "maxBytesPerChar" => Some(Some(V::F32(4.0))),
            "isAutoDetecting" | "isCharsetDetected" => Some(Some(V::I32(0))),
            "decode" | "decodeLoop" => Some(Some(charset_decode(vm, args))),
            "flush" => Some(Some(host(
                vm,
                "Ljava/nio/charset/CoderResult;",
                HostState::StrBuf("UNDERFLOW".into()),
            ))),
            _ => None,
        };
    }
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
        "newDecoder" | "newEncoder" => {
            // Carry the receiver's charset name so ISO-8859-1 et al.
            // decode/encode under their own mapping, not UTF-8.
            let n = match args[0] {
                V::Ref(h) => s(vm, h).unwrap_or_else(|| "UTF-8".into()),
                _ => "UTF-8".into(),
            };
            let desc = if name == "newDecoder" {
                "Ljava/nio/charset/CharsetDecoder;"
            } else {
                "Ljava/nio/charset/CharsetEncoder;"
            };
            Some(Some(host(vm, desc, HostState::StrBuf(n))))
        }
        "decode" => Some(Some(charset_decode(vm, args))),
        "encode" => {
            // `encode(CharBuffer|CharSequence)` → ByteBuffer in `cs`.
            let cs = match args[0] {
                V::Ref(h) => charset_name(vm, h),
                _ => "UTF8".into(),
            };
            // `s()` covers String/CharSequence and CharBuffer content.
            let text = match args.get(1) {
                Some(&V::Ref(h)) => s(vm, h).unwrap_or_default(),
                _ => String::new(),
            };
            // `byte[]` elements are signed — bytes ≥0x80 store as
            // negative ints like every other byte writer here.
            let data: Vec<V> = if cs.contains("88591") || cs.contains("LATIN1") {
                text.chars()
                    .map(|c| {
                        V::I32(if (c as u32) < 0x100 {
                            (c as u8) as i8 as i32
                        } else {
                            b'?' as i32
                        })
                    })
                    .collect()
            } else if cs.contains("ASCII") {
                text.chars()
                    .map(|c| {
                        V::I32(if (c as u32) < 0x80 {
                            (c as u8) as i8 as i32
                        } else {
                            b'?' as i32
                        })
                    })
                    .collect()
            } else {
                text.as_bytes()
                    .iter()
                    .map(|b| V::I32(*b as i8 as i32))
                    .collect()
            };
            let arr = vm.heap.alloc(Obj::Array {
                elem: "B".into(),
                data,
            });
            Some(Some(host(
                vm,
                "Ljava/nio/ByteBuffer;",
                HostState::Slot(V::Ref(arr)),
            )))
        }
        "aliases" | "availableCharsets" | "isSupported" | "contains" | "canEncode" => {
            if name == "isSupported" || name == "canEncode" || name == "contains" {
                Some(Some(V::I32(1)))
            } else if name == "availableCharsets" {
                Some(Some(host(vm, "Ljava/util/Map;", HostState::Map(vec![]))))
            } else {
                Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![]))))
            }
        }
        _ => None,
    }
}

fn nio_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
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
    let bpos =
        |vm: &Vm| i32v(pseudo_get(vm, this, P_BBUF_POS).unwrap_or(V::I32(0))).max(0) as usize;
    let little = |vm: &Vm| i32v(pseudo_get(vm, this, P_BBUF_ORD).unwrap_or(V::I32(0))) != 0;
    // Typed views (`asIntBuffer`…) keep the same byte backing but
    // address it in elements of `esz` bytes.
    let esz =
        |vm: &Vm| i32v(pseudo_get(vm, this, P_BBUF_ESZ).unwrap_or(V::I32(1))).clamp(1, 8) as usize;
    // A typed view's content starts at the parent's byte position when
    // it was created — `read_at`/`put` index `base + pos`.
    let base =
        |vm: &Vm| i32v(pseudo_get(vm, this, P_BBUF_BASE).unwrap_or(V::I32(0))).max(0) as usize;
    // Capacity in element units — a view (slice/duplicate) carries
    // its own bound in P_BBUF_CAP; otherwise it spans to the backing
    // array's end minus the byte offset.
    let blim_capacity = |vm: &Vm| -> usize {
        if let Some(c) = pseudo_get(vm, this, P_BBUF_CAP) {
            return i32v(c).max(0) as usize;
        }
        backing(vm)
            .and_then(|b| match vm.heap.get(b) {
                Some(Obj::Array { data, .. }) => Some(data.len()),
                _ => None,
            })
            .unwrap_or(0)
            .saturating_sub(base(vm))
            / esz(vm)
    };
    // Limit in element units — absent means capacity.
    let blim = |vm: &Vm| -> usize {
        match pseudo_get(vm, this, P_BBUF_LIM) {
            Some(v) => i32v(v).max(0) as usize,
            None => blim_capacity(vm),
        }
    };
    // Read `n` bytes at `pos`; None when out of bounds. `pos` is a byte
    // offset; the limit bounds it (elements → bytes via `esz`).
    let read_at = |vm: &Vm, pos: usize, n: usize| -> Option<Vec<u8>> {
        let pos = base(vm) + pos;
        let lim_bytes = base(vm) + blim(vm) * esz(vm);
        let b = backing(vm)?;
        match vm.heap.get(b) {
            Some(Obj::Array { data, .. }) => {
                if pos + n > data.len() || pos + n > lim_bytes {
                    None
                } else {
                    Some(data[pos..pos + n].iter().map(|v| i32v(*v) as u8).collect())
                }
            }
            _ => None,
        }
    };
    match name {
        "toString" if class.contains("CharBuffer") => {
            // CharBuffer.toString() yields the remaining chars, not an
            // object identity — `Charset.decode(b).toString()` needs it.
            Some(Some(jstr(
                vm,
                &charbuf_string(vm, this).unwrap_or_default(),
            )))
        }
        "allocate" | "allocateDirect" => {
            // `FloatBuffer.allocate(n)` allocates n ELEMENTS — the
            // backing byte array is n * esz bytes.
            let (desc, w) = match class {
                c if c.contains("IntBuffer") => ("Ljava/nio/IntBuffer;", 4),
                c if c.contains("FloatBuffer") => ("Ljava/nio/FloatBuffer;", 4),
                c if c.contains("LongBuffer") => ("Ljava/nio/LongBuffer;", 8),
                c if c.contains("DoubleBuffer") => ("Ljava/nio/DoubleBuffer;", 8),
                c if c.contains("ShortBuffer") => ("Ljava/nio/ShortBuffer;", 2),
                c if c.contains("CharBuffer") => ("Ljava/nio/CharBuffer;", 2),
                _ => ("Ljava/nio/ByteBuffer;", 1),
            };
            let n = i32v(
                *args
                    .get(if args.len() > 1 { 1 } else { 0 })
                    .unwrap_or(&V::I32(0)),
            );
            let buf = vm.heap.alloc(Obj::Array {
                elem: "B".into(),
                data: vec![V::I32(0); (n.max(0) as usize) * w],
            });
            let v = host(vm, desc, HostState::Slot(V::Ref(buf)));
            if let (V::Ref(h), true) = (v, w > 1) {
                pseudo_set(vm, h, P_BBUF_ESZ, V::I32(w as i32));
            }
            Some(Some(v))
        }
        "asIntBuffer" | "asFloatBuffer" | "asLongBuffer" | "asDoubleBuffer" | "asShortBuffer"
        | "asCharBuffer" => {
            // Typed view over the same byte backing; positions/capacity
            // are element-indexed via P_BBUF_ESZ.
            let (desc, w) = match name {
                "asIntBuffer" => ("Ljava/nio/IntBuffer;", 4),
                "asFloatBuffer" => ("Ljava/nio/FloatBuffer;", 4),
                "asLongBuffer" => ("Ljava/nio/LongBuffer;", 8),
                "asDoubleBuffer" => ("Ljava/nio/DoubleBuffer;", 8),
                "asShortBuffer" => ("Ljava/nio/ShortBuffer;", 2),
                _ => ("Ljava/nio/CharBuffer;", 2),
            };
            let b = backing(vm).map(V::Ref).unwrap_or(V::I32(0));
            let base0 = base(vm) + bpos(vm) * esz(vm);
            let v = host(vm, desc, HostState::Slot(b));
            if let V::Ref(h) = v {
                pseudo_set(vm, h, P_BBUF_ESZ, V::I32(w));
                if base0 != 0 {
                    pseudo_set(vm, h, P_BBUF_BASE, V::I32(base0 as i32));
                }
                if let Some(o) = pseudo_get(vm, this, P_BBUF_ORD) {
                    pseudo_set(vm, h, P_BBUF_ORD, o);
                }
            }
            Some(Some(v))
        }
        "wrap" => {
            // wrap(byte[]) / wrap(byte[], off, len) — keep the array so
            // `array()` returns the same handle writes went into.
            // P_BBUF_RAW marks the content as foreign-supplied bytes
            // (native order until `order()` says otherwise); `slice`/
            // `duplicate` return this handle so the mark carries over.
            let b = args.first().copied().unwrap_or(V::Ref(0));
            let v = host(vm, "Ljava/nio/ByteBuffer;", HostState::Slot(b));
            if let V::Ref(h) = v {
                pseudo_set(vm, h, P_BBUF_RAW, V::I32(1));
                if args.len() > 2 {
                    pseudo_set(vm, h, P_BBUF_POS, V::I32(i32v(args[1])));
                    // wrap(arr, off, len): limit = off + len.
                    pseudo_set(
                        vm,
                        h,
                        P_BBUF_LIM,
                        V::I32(i32v(args[1]) + i32v(args.get(2).copied().unwrap_or(V::I32(0)))),
                    );
                }
            }
            Some(Some(v))
        }
        "order" => {
            if crate::dbg_flag("OVELO_ORDDBG") {
                eprintln!(
                    "[orddbg] order arg={:?} heap={:?}",
                    args.get(1),
                    match args.get(1) {
                        Some(&V::Ref(h)) => vm
                            .heap
                            .get(h)
                            .map(|o| format!("{:?}", o))
                            .unwrap_or("?".into()),
                        _ => "not-ref".to_string(),
                    }
                );
            }
            let ord = match args.get(1) {
                Some(&V::Ref(bo)) => match vm.heap.get(bo) {
                    Some(Obj::Host {
                        state: HostState::Slot(v),
                        ..
                    }) => *v,
                    // `nativeOrder()` (little on our targets) and any
                    // unknown ByteOrder object — ARM64 is little-endian.
                    _ => V::I32(1),
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
        "hasArray" => Some(Some(V::I32(1))),
        "hasRemaining" => Some(Some(V::I32((blim(vm) > bpos(vm)) as i32))),
        // Every buffer we mint is array-backed but behaves direct —
        // guests pool-vs-copy on this (Korge picks a shared pool when
        // a buffer reports non-direct, splitting read/write sites).
        "isDirect" => Some(Some(V::I32(1))),
        "isReadOnly" => Some(Some(V::I32(0))),
        "arrayOffset" => Some(Some(V::I32(0))),
        "capacity" => {
            let n = backing(vm)
                .and_then(|b| match vm.heap.get(b) {
                    Some(Obj::Array { data, .. }) => Some(data.len()),
                    _ => None,
                })
                .unwrap_or(0)
                .saturating_sub(base(vm))
                / esz(vm);
            Some(Some(V::I32(n as i32)))
        }
        "limit" => {
            if args.len() > 1 {
                pseudo_set(vm, this, P_BBUF_LIM, args[1]);
                Some(Some(args[0]))
            } else {
                Some(Some(V::I32(blim(vm) as i32)))
            }
        }
        "remaining" => Some(Some(V::I32(blim(vm).saturating_sub(bpos(vm)) as i32))),
        "position" => {
            if args.len() > 1 {
                if crate::dbg_flag("OVELO_NIODBG") {
                    eprintln!("[nio] position#{this} {} -> {}", bpos(vm), i32v(args[1]));
                }
                pseudo_set(vm, this, P_BBUF_POS, args[1]);
                Some(Some(args[0]))
            } else {
                Some(Some(V::I32(bpos(vm) as i32)))
            }
        }
        "flip" => {
            // limit = position, position = 0 — the written region
            // becomes the readable one.
            let pos = bpos(vm) as i32;
            if crate::dbg_flag("OVELO_NIODBG") {
                eprintln!("[nio] flip#{this} pos={pos} lim={}", blim(vm));
            }
            pseudo_set(vm, this, P_BBUF_LIM, V::I32(pos));
            pseudo_set(vm, this, P_BBUF_POS, V::I32(0));
            Some(Some(args[0]))
        }
        "clear" => {
            // position = 0, limit = capacity.
            pseudo_set(vm, this, P_BBUF_POS, V::I32(0));
            pseudo_set(vm, this, P_BBUF_LIM, V::I32(blim_capacity(vm) as i32));
            Some(Some(args[0]))
        }
        "rewind" => {
            pseudo_set(vm, this, P_BBUF_POS, V::I32(0));
            Some(Some(args[0]))
        }
        "reset" => {
            // position = mark (0 when none was taken).
            let m = i32v(pseudo_get(vm, this, P_BBUF_MARK).unwrap_or(V::I32(0)));
            pseudo_set(vm, this, P_BBUF_POS, V::I32(m.max(0)));
            Some(Some(args[0]))
        }
        "nativeOrder" => {
            let h = vm.byteorder_h.get();
            let v = if h != 0 && vm.heap.get(h).is_some() {
                V::Ref(h)
            } else {
                let v = host(vm, "Ljava/nio/ByteOrder;", HostState::Slot(V::I32(1)));
                if let V::Ref(hh) = v {
                    vm.byteorder_h.set(hh);
                }
                v
            };
            Some(Some(v))
        }
        "mark" => {
            pseudo_set(vm, this, P_BBUF_MARK, V::I32(bpos(vm) as i32));
            Some(Some(args[0]))
        }
        "compact" => {
            // Move the unread region [pos, limit) to the front, then
            // position = remaining, limit = capacity — the caller reads
            // into the tail and `flip`s back to read mode.
            let es = esz(vm);
            let (pos, lim) = (bpos(vm), blim(vm));
            let cap = blim_capacity(vm);
            if crate::dbg_flag("OVELO_NIODBG") {
                eprintln!("[nio] compact#{this} pos={pos} lim={lim} cap={cap}");
            }
            if let Some(bh) = backing(vm).filter(|_| lim > pos) {
                let bb = base(vm);
                if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                    for i in 0..(lim - pos) * es {
                        let src = data.get(bb + pos * es + i).copied().unwrap_or(V::I32(0));
                        if let Some(slot) = data.get_mut(bb + i) {
                            *slot = src;
                        }
                    }
                }
            }
            pseudo_set(vm, this, P_BBUF_POS, V::I32((lim - pos) as i32));
            pseudo_set(vm, this, P_BBUF_LIM, V::I32(cap as i32));
            Some(Some(args[0]))
        }
        "slice" | "duplicate" | "asReadOnlyBuffer" => {
            // A view buffer is a NEW object sharing the same backing:
            // its own position/limit must not alias the parent's.
            // `slice` re-bases to the parent's current position;
            // `duplicate`/`asReadOnlyBuffer` copy position/limit as-is.
            // (ByteBuffer order matters little — our element model
            // reads via esz/ord flags, which carry over.)
            let desc = match vm.heap.get(this) {
                Some(Obj::Host { desc, .. }) => desc.clone(),
                _ => "Ljava/nio/ByteBuffer;".to_string(),
            };
            let (pos, lim, parent_cap) = (bpos(vm), blim(vm), blim_capacity(vm));
            let es = esz(vm);
            let base_new = if name == "slice" {
                base(vm) + pos * es
            } else {
                base(vm)
            };
            let b = backing(vm).map(V::Ref).unwrap_or(V::I32(0));
            let v = host(vm, &desc, HostState::Slot(b));
            if let V::Ref(h) = v {
                if base_new != 0 {
                    pseudo_set(vm, h, P_BBUF_BASE, V::I32(base_new as i32));
                }
                if es != 1 {
                    pseudo_set(vm, h, P_BBUF_ESZ, V::I32(es as i32));
                }
                if let Some(o) = pseudo_get(vm, this, P_BBUF_ORD) {
                    pseudo_set(vm, h, P_BBUF_ORD, o);
                }
                if pseudo_get(vm, this, P_BBUF_RAW).is_some() {
                    pseudo_set(vm, h, P_BBUF_RAW, V::I32(1));
                }
                if name == "slice" {
                    // slice: position 0, limit AND capacity = the
                    // parent's remaining — a clear() must not reopen
                    // the parent's tail beyond the sliced window.
                    let rem = lim.saturating_sub(pos);
                    pseudo_set(vm, h, P_BBUF_POS, V::I32(0));
                    pseudo_set(vm, h, P_BBUF_LIM, V::I32(rem as i32));
                    pseudo_set(vm, h, P_BBUF_CAP, V::I32(rem as i32));
                } else {
                    // duplicate: same position/limit, same window.
                    pseudo_set(vm, h, P_BBUF_POS, V::I32(pos as i32));
                    pseudo_set(
                        vm,
                        h,
                        P_BBUF_LIM,
                        V::I32(if lim == parent_cap { parent_cap } else { lim } as i32),
                    );
                    if pseudo_get(vm, this, P_BBUF_CAP).is_some() {
                        pseudo_set(vm, h, P_BBUF_CAP, V::I32(parent_cap as i32));
                    }
                }
            }
            Some(Some(v))
        }
        "putInt" | "putLong" | "putShort" | "putDouble" | "putFloat" | "putChar" | "putByte"
        | "put" => {
            let (pos0, es) = (bpos(vm), esz(vm));
            // One V -> `es` bytes, endian-adjusted like `put`'s reverse.
            let elem_bytes = |vm: &Vm, v: V| -> Vec<u8> {
                let mut b = match (v, es) {
                    (V::F32(f), 4) => f.to_be_bytes().to_vec(),
                    (V::F64(d), 8) => d.to_be_bytes().to_vec(),
                    (V::I64(l), 8) => l.to_be_bytes().to_vec(),
                    (_, 4) => i32v(v).to_be_bytes().to_vec(),
                    (_, 2) => (i32v(v) as u16).to_be_bytes().to_vec(),
                    _ => vec![i32v(v) as u8],
                };
                if little(vm) {
                    b.reverse();
                }
                b
            };
            let put = |vm: &mut Vm, pos: usize, bytes: &[u8]| {
                let mut b = bytes.to_vec();
                if little(vm) {
                    b.reverse();
                }
                let pos = base(vm) + pos;
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
                        // put(index, byte) — absolute index inside
                        // this view's window, so re-base like read_at
                        // does (a slice's index 0 is backing[base]).
                        let pos = base(vm) + i32v(args[1]).max(0) as usize * es;
                        // elem_bytes is already endian-adjusted — the
                        // `put` closure would reverse it again.
                        let x = elem_bytes(vm, args[2]);
                        if let Some(bh) = backing(vm) {
                            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                                for (i, b) in x.iter().enumerate() {
                                    if let Some(slot) = data.get_mut(pos + i) {
                                        *slot = V::I32(*b as i8 as i32);
                                    }
                                }
                            }
                        }
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
                                    .flat_map(|v| elem_bytes(vm, *v))
                                    .collect(),
                                _ => Vec::new(),
                            },
                            _ => Vec::new(),
                        };
                        if crate::dbg_flag("OVELO_ORDDBG") {
                            eprintln!(
                                "[orddbg] bulk-put class={class} es={es} little={} ord={:?} nbytes={}",
                                little(vm),
                                pseudo_get(vm, this, P_BBUF_ORD),
                                bytes.len()
                            );
                        }
                        if let Some(bh) = backing(vm) {
                            let bb = base(vm);
                            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                                for (i, x) in bytes.iter().enumerate() {
                                    if let Some(slot) = data.get_mut(bb + pos0 * es + i) {
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
                                    data.iter().flat_map(|v| elem_bytes(vm, *v)).collect()
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
                                        let ses = i32v(
                                            pseudo_get(vm, sh, P_BBUF_ESZ).unwrap_or(V::I32(1)),
                                        )
                                        .clamp(1, 8)
                                            as usize;
                                        let sp = i32v(
                                            pseudo_get(vm, sh, P_BBUF_POS).unwrap_or(V::I32(0)),
                                        )
                                        .max(0)
                                            as usize
                                            * ses;
                                        let sb_off = i32v(
                                            pseudo_get(vm, sh, P_BBUF_BASE).unwrap_or(V::I32(0)),
                                        )
                                        .max(0)
                                            as usize;
                                        if let Some(Obj::Array { data, .. }) = vm.heap.get(sb) {
                                            v = data
                                                .iter()
                                                .skip(sb_off + sp)
                                                .map(|x| i32v(*x) as u8)
                                                .collect();
                                        }
                                        let n = v.len();
                                        pseudo_set(
                                            vm,
                                            sh,
                                            P_BBUF_POS,
                                            V::I32((sp / ses + n / ses) as i32),
                                        );
                                    }
                                    v
                                }
                            },
                            v => elem_bytes(vm, v.unwrap_or(V::I32(0))),
                        };
                        let len = bytes.len();
                        if let Some(bh) = backing(vm) {
                            let bb = base(vm);
                            if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(bh) {
                                for (i, x) in bytes.iter().enumerate() {
                                    if let Some(slot) = data.get_mut(bb + pos0 * es + i) {
                                        *slot = V::I32(*x as i8 as i32);
                                    }
                                }
                            }
                        }
                        pseudo_set(vm, this, P_BBUF_POS, V::I32((pos0 + len / es) as i32));
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
                        (i32v(args[1]).max(0) as usize * es, 0)
                    } else {
                        (pos0 * es, bytes.len() / es)
                    };
                    let v2 = if args.len() > 2 { args[2] } else { v };
                    let bytes = if args.len() > 2 {
                        match name {
                            "putInt" => i32v(v2).to_be_bytes().to_vec(),
                            "putLong" => i64v(v2).to_be_bytes().to_vec(),
                            "putDouble" => f64v(v2).to_be_bytes().to_vec(),
                            "putFloat" => (f64v(v2) as f32).to_be_bytes().to_vec(),
                            "putShort" | "putChar" => (i32v(v2) as u16).to_be_bytes().to_vec(),
                            _ => vec![i32v(v2) as u8],
                        }
                    } else {
                        bytes
                    };
                    put(vm, pos, &bytes);
                    if adv > 0 {
                        pseudo_set(vm, this, P_BBUF_POS, V::I32((pos0 + adv) as i32));
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
            let es = esz(vm);
            let pos = if args.len() > 1 && matches!(args[1], V::I32(_) | V::I64(_)) {
                i32v(args[1]).max(0) as usize * es
            } else {
                bpos(vm) * es
            };
            if name == "get" && args.len() > 1 && matches!(args[1], V::Ref(_)) {
                // get(dst[], off, len) — bulk read; `len` is in
                // elements for typed views.
                let (dst, off, len) = (
                    args[1],
                    args.get(2).map(|v| i32v(*v).max(0) as usize).unwrap_or(0),
                    args.get(3)
                        .map(|v| i32v(*v).max(0) as usize)
                        .unwrap_or(usize::MAX),
                );
                let is_float = matches!(
                    vm.heap.get(this),
                    Some(Obj::Host { desc, .. }) if desc.contains("Float")
                );
                if let Some(bytes) = read_at(vm, pos, len.min(usize::MAX / 8).saturating_mul(es)) {
                    let n = bytes.len();
                    let le = little(vm);
                    if let V::Ref(dh) = dst {
                        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(dh) {
                            for (i, slot) in data.iter_mut().enumerate().skip(off).take(len) {
                                let at = (i - off) * es;
                                let mut e = vec![0u8; es];
                                if let Some(src) = bytes.get(at..at + es) {
                                    e.copy_from_slice(src);
                                }
                                if le {
                                    e.reverse();
                                }
                                *slot = match (es, is_float) {
                                    (8, true) => V::F64(f64::from_be_bytes(
                                        e[..8].try_into().unwrap_or([0; 8]),
                                    )),
                                    (8, false) => V::I64(i64::from_be_bytes(
                                        e[..8].try_into().unwrap_or([0; 8]),
                                    )),
                                    (4, true) => V::F32(f32::from_be_bytes(
                                        e[..4].try_into().unwrap_or([0; 4]),
                                    )),
                                    (4, false) => V::I32(i32::from_be_bytes(
                                        e[..4].try_into().unwrap_or([0; 4]),
                                    )),
                                    (2, _) => V::I32(i16::from_be_bytes(
                                        e[..2].try_into().unwrap_or([0; 2]),
                                    ) as i32),
                                    _ => V::I32(e[0] as i8 as i32),
                                };
                            }
                        }
                    }
                    pseudo_set(vm, this, P_BBUF_POS, V::I32((pos / es + n / es) as i32));
                }
                return Some(Some(args[0]));
            }
            // A `get()`/`get(index)` on a typed view reads one element.
            let size = if es > 1 && (name == "get" || name == "getByte") {
                es
            } else {
                size
            };
            let is_float = matches!(
                vm.heap.get(this),
                Some(Obj::Host { desc, .. }) if desc.contains("Float")
            );
            let mut bytes = read_at(vm, pos, size).unwrap_or_else(|| vec![0; size]);
            if little(vm) {
                bytes.reverse();
            }
            if args.len() == 1 {
                pseudo_set(vm, this, P_BBUF_POS, V::I32((pos / es + size / es) as i32));
            }
            let v = match name {
                "get" if es > 1 => match (es, is_float) {
                    (8, true) => {
                        V::F64(f64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])))
                    }
                    (8, false) => {
                        V::I64(i64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])))
                    }
                    (4, true) => {
                        V::F32(f32::from_be_bytes(bytes[..4].try_into().unwrap_or([0; 4])))
                    }
                    (4, false) => {
                        V::I32(i32::from_be_bytes(bytes[..4].try_into().unwrap_or([0; 4])))
                    }
                    (2, _) => {
                        V::I32(i16::from_be_bytes(bytes[..2].try_into().unwrap_or([0; 2])) as i32)
                    }
                    _ => V::I32(bytes[0] as i8 as i32),
                },
                "getInt" => V::I32(i32::from_be_bytes(bytes[..4].try_into().unwrap_or([0; 4]))),
                "getLong" => V::I64(i64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8]))),
                "getDouble" => V::F64(f64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8]))),
                "getFloat" => V::F32(f32::from_be_bytes(bytes[..4].try_into().unwrap_or([0; 4]))),
                "getShort" => {
                    V::I32(i16::from_be_bytes(bytes[..2].try_into().unwrap_or([0; 2])) as i32)
                }
                "getChar" => {
                    V::I32(u16::from_be_bytes(bytes[..2].try_into().unwrap_or([0; 2])) as i32)
                }
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

const PK_LOCALE_TAG: u32 = 0xFFFD_FF80;

pub(crate) fn locale_tag(vm: &Vm, h: u32) -> String {
    if let Some(v) = pseudo_get(vm, h, PK_LOCALE_TAG) {
        if let Some(t) = sv(vm, v) {
            return t;
        }
    }
    s(vm, h).unwrap_or_else(|| "en-US".into())
}

fn locale_piece(tag: &str, want_script: bool) -> String {
    let parts: Vec<&str> = tag.split(['-', '_']).filter(|p| !p.is_empty()).collect();
    if want_script {
        return parts
            .iter()
            .skip(1)
            .find(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic()))
            .map(|p| p.to_string())
            .unwrap_or_default();
    }
    parts
        .first()
        .map(|p| p.to_ascii_lowercase())
        .unwrap_or_default()
}

fn locale_construct(vm: &mut Vm, args: &[V]) -> Option<V> {
    let V::Ref(h) = args.first().copied()? else {
        return None;
    };
    let mut parts = Vec::new();
    for v in &args[1..] {
        if let Some(t) = sv(vm, *v) {
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    let tag = if parts.is_empty() {
        "en-US".into()
    } else {
        parts.join("-")
    };
    let tv = jstr(vm, &tag);
    pseudo_set(vm, h, PK_LOCALE_TAG, tv);
    None
}

fn misc_fmt(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    if class == "Ljava/util/Locale;" {
        return match name {
            "<init>" => return Some(locale_construct(vm, args)),
            "getDefault" | "getDefault0" | "ROOT" => Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("en-US".into()),
            )),
            "forLanguageTag" => {
                let tag = args
                    .first()
                    .and_then(|v| sv(vm, *v))
                    .unwrap_or_else(|| "en-US".into());
                let h = host(vm, "Ljava/util/Locale;", HostState::StrBuf(tag.clone()));
                if let V::Ref(hh) = h {
                    let tv = jstr(vm, &tag);
                    pseudo_set(vm, hh, PK_LOCALE_TAG, tv);
                }
                Some(h)
            }
            "getLanguage" => {
                let h = match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                };
                Some(jstr(vm, &locale_piece(&locale_tag(vm, h), false)))
            }
            "getScript" => {
                let h = match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                };
                Some(jstr(vm, &locale_piece(&locale_tag(vm, h), true)))
            }
            "getCountry" | "getVariant" => {
                let h = match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                };
                let tag = locale_tag(vm, h);
                let part = tag
                    .split(['-', '_'])
                    .filter(|p| !p.is_empty())
                    .skip(1)
                    .find(|p| !(p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic())))
                    .unwrap_or("");
                Some(jstr(vm, part))
            }
            "toString" | "toLanguageTag" | "getDisplayName" | "getDisplayLanguage"
            | "getDisplayCountry" => {
                let h = match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                };
                let t = locale_tag(vm, h);
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
    if class == "Ljava/text/Normalizer;" {
        return match name {
            // NFC/NFD identity is wrong for real combining sequences,
            // but OkHttp's host canonicalization only needs the ASCII
            // fast path — return the input unchanged.
            "normalize" => Some(args[0]),
            "isNormalized" => Some(V::I32(1)),
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

fn replace_host_state(vm: &mut Vm, this: V, class: &str, state: HostState) {
    let V::Ref(this) = this else { return };
    let (ci, d, fs) = match vm.heap.get(this) {
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
    if let Some(o) = vm.heap.get_mut(this) {
        *o = Obj::Host {
            class_idx: ci,
            desc: d,
            fields: fs,
            state,
        };
    }
}

fn digest_new(algorithm: &str) -> Option<DigestState> {
    let alg = algorithm.to_ascii_uppercase().replace('-', "");
    match alg.as_str() {
        "SHA1" => Some(DigestState::Sha1(sha1::Sha1::new())),
        "SHA224" => None,
        "SHA256" => Some(DigestState::Sha256(sha2::Sha256::new())),
        "SHA384" => Some(DigestState::Sha384(sha2::Sha384::new())),
        "SHA512" => Some(DigestState::Sha512(sha2::Sha512::new())),
        _ => None,
    }
}

fn digest_like(state: &DigestState) -> DigestState {
    match state {
        DigestState::Sha1(_) => DigestState::Sha1(sha1::Sha1::new()),
        DigestState::Sha256(_) => DigestState::Sha256(sha2::Sha256::new()),
        DigestState::Sha384(_) => DigestState::Sha384(sha2::Sha384::new()),
        DigestState::Sha512(_) => DigestState::Sha512(sha2::Sha512::new()),
    }
}

fn digest_algorithm(state: &DigestState) -> (&'static str, i32) {
    match state {
        DigestState::Sha1(_) => ("SHA-1", 20),
        DigestState::Sha256(_) => ("SHA-256", 32),
        DigestState::Sha384(_) => ("SHA-384", 48),
        DigestState::Sha512(_) => ("SHA-512", 64),
    }
}

fn digest_update(state: &mut DigestState, bytes: &[u8]) {
    match state {
        DigestState::Sha1(d) => d.update(bytes),
        DigestState::Sha256(d) => d.update(bytes),
        DigestState::Sha384(d) => d.update(bytes),
        DigestState::Sha512(d) => d.update(bytes),
    }
}

fn digest_finish(state: &mut DigestState) -> Vec<u8> {
    match state {
        DigestState::Sha1(d) => d.finalize_reset().to_vec(),
        DigestState::Sha256(d) => d.finalize_reset().to_vec(),
        DigestState::Sha384(d) => d.finalize_reset().to_vec(),
        DigestState::Sha512(d) => d.finalize_reset().to_vec(),
    }
}

fn array_len(vm: &Vm, array: V) -> usize {
    let V::Ref(h) = array else {
        return 0;
    };
    match vm.heap.get(h) {
        Some(Obj::Array { data, .. }) => data.len(),
        _ => 0,
    }
}

fn array_bytes(vm: &Vm, array: V, offset: usize, len: Option<usize>) -> Vec<u8> {
    let V::Ref(h) = array else {
        return Vec::new();
    };
    let Some(Obj::Array { data, .. }) = vm.heap.get(h) else {
        return Vec::new();
    };
    let start = offset.min(data.len());
    let end = len
        .map(|len| start.saturating_add(len).min(data.len()))
        .unwrap_or(data.len());
    data[start..end].iter().map(|v| i32v(*v) as u8).collect()
}

fn key_spec_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(Some(match name {
        "<init>" => {
            let (bytes, algorithm) = if args.len() >= 5 {
                let array = args.get(1).copied().unwrap_or(V::Ref(0));
                let offset = i32v(args[2]);
                let len = i32v(args[3]);
                let array_len = array_len(vm, array);
                let start = offset.max(0) as usize;
                if offset < 0
                    || len < 0
                    || start > array_len
                    || len as usize > array_len.saturating_sub(start)
                {
                    return Err(vm.throw_builtin(
                        "Ljava/lang/IllegalArgumentException;",
                        "key range outside array",
                    ));
                }
                (
                    array_bytes(vm, array, start, Some(len as usize)),
                    args.get(4).and_then(|v| sv(vm, *v)).unwrap_or_default(),
                )
            } else {
                (
                    array_bytes(vm, args.get(1).copied().unwrap_or(V::Ref(0)), 0, None),
                    args.get(2).and_then(|v| sv(vm, *v)).unwrap_or_default(),
                )
            };
            replace_host_state(
                vm,
                args[0],
                "Ljavax/crypto/spec/SecretKeySpec;",
                HostState::Key { algorithm, bytes },
            );
            None
        }
        "getEncoded" => {
            let bytes = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Key { bytes, .. },
                        ..
                    }) => bytes.clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            Some(byte_arr(vm, &bytes))
        }
        "getAlgorithm" => {
            let algorithm = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Key { algorithm, .. },
                        ..
                    }) => algorithm.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            Some(jstr(vm, &algorithm))
        }
        _ => return Ok(None),
    }))
}

fn mac_algorithm(algorithm: &str) -> Option<String> {
    let normalized = algorithm.to_ascii_uppercase().replace(['-', '_'], "");
    matches!(
        normalized.as_str(),
        "HMACSHA1" | "HMACSHA256" | "HMACSHA384" | "HMACSHA512"
    )
    .then_some(normalized)
}

fn mac_output_len(algorithm: &str) -> usize {
    match algorithm {
        "HMACMD5" => 16,
        "HMACSHA1" => 20,
        "HMACSHA256" => 32,
        "HMACSHA384" => 48,
        "HMACSHA512" => 64,
        _ => 0,
    }
}

fn mac_compute(algorithm: &str, key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    macro_rules! compute {
        ($digest:ty) => {{
            let mut mac = hmac::Hmac::<$digest>::new_from_slice(key).ok()?;
            mac.update(data);
            Some(mac.finalize().into_bytes().to_vec())
        }};
    }
    match algorithm {
        "HMACMD5" => compute!(md5::Md5),
        "HMACSHA1" => compute!(sha1::Sha1),
        "HMACSHA256" => compute!(sha2::Sha256),
        "HMACSHA384" => compute!(sha2::Sha384),
        "HMACSHA512" => compute!(sha2::Sha512),
        _ => None,
    }
}

fn consume_mac_input(vm: &mut Vm, args: &[V], consumed: usize) {
    if args.len() != 2 {
        return;
    }
    let Some(V::Ref(h)) = args.get(1).copied() else {
        return;
    };
    let is_buffer = matches!(
        vm.heap.get(h),
        Some(Obj::Host {
            state: HostState::Slot(V::Ref(_)),
            ..
        })
    );
    if is_buffer {
        let (_, pos) = bb_bytes(vm, h);
        pseudo_set(vm, h, P_BBUF_POS, V::I32((pos + consumed) as i32));
    }
}

fn mac_input_bytes(vm: &Vm, args: &[V]) -> Vec<u8> {
    match args.len() {
        2 => match args.get(1).copied().unwrap_or(V::I32(0)) {
            v @ V::Ref(h) => match vm.heap.get(h) {
                Some(Obj::Host {
                    state: HostState::Slot(V::Ref(_)),
                    ..
                }) => bb_bytes(vm, h).0,
                _ => array_bytes(vm, v, 0, None),
            },
            v => vec![i32v(v) as u8],
        },
        4 => array_bytes(
            vm,
            args.get(1).copied().unwrap_or(V::Ref(0)),
            i32v(args[2]).max(0) as usize,
            Some(i32v(args[3]).max(0) as usize),
        ),
        _ => Vec::new(),
    }
}

fn mac_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(Some(match name {
        "getInstance" => {
            let algorithm = args.first().and_then(|v| sv(vm, *v)).unwrap_or_default();
            let Some(algorithm) = mac_algorithm(&algorithm) else {
                return Err(vm.throw_builtin(
                    "Ljava/security/NoSuchAlgorithmException;",
                    "unsupported Mac algorithm",
                ));
            };
            Some(host(
                vm,
                "Ljavax/crypto/Mac;",
                HostState::Mac {
                    algorithm,
                    key: Vec::new(),
                    data: Vec::new(),
                },
            ))
        }
        "init" => {
            let key = match args.get(1).copied() {
                Some(V::Ref(h)) => match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Key { bytes, .. },
                        ..
                    }) => bytes.clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if key.is_empty() {
                return Err(vm.throw_builtin("Ljava/security/InvalidKeyException;", "empty key"));
            }
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host {
                    state: HostState::Mac { key: k, data, .. },
                    ..
                }) = vm.heap.get_mut(*h)
                {
                    *k = key;
                    data.clear();
                }
            }
            None
        }
        "update" => {
            let bytes = mac_input_bytes(vm, args);
            consume_mac_input(vm, args, bytes.len());
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host {
                    state: HostState::Mac { data, .. },
                    ..
                }) = vm.heap.get_mut(*h)
                {
                    data.extend_from_slice(&bytes);
                }
            }
            None
        }
        "doFinal" => {
            let output_target = args.len() == 3;
            if output_target {
                let required = match args.first() {
                    Some(V::Ref(h)) => match vm.heap.get(*h) {
                        Some(Obj::Host {
                            state: HostState::Mac { algorithm, .. },
                            ..
                        }) => mac_output_len(algorithm),
                        _ => 0,
                    },
                    _ => 0,
                };
                let offset = i32v(args[2]);
                let capacity = array_len(vm, args[1]);
                if offset < 0
                    || offset as usize > capacity
                    || required > capacity.saturating_sub(offset as usize)
                {
                    return Err(vm.throw_builtin(
                        "Ljavax/crypto/ShortBufferException;",
                        "output buffer too small",
                    ));
                }
            }
            if args.len() == 2 {
                let bytes = mac_input_bytes(vm, args);
                consume_mac_input(vm, args, bytes.len());
                if let Some(V::Ref(h)) = args.first() {
                    if let Some(Obj::Host {
                        state: HostState::Mac { data, .. },
                        ..
                    }) = vm.heap.get_mut(*h)
                    {
                        data.extend_from_slice(&bytes);
                    }
                }
            }
            let state = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get_mut(*h) {
                    Some(Obj::Host {
                        state:
                            HostState::Mac {
                                algorithm,
                                key,
                                data,
                            },
                        ..
                    }) => {
                        let state = (algorithm.clone(), key.clone(), data.clone());
                        data.clear();
                        state
                    }
                    _ => (String::new(), Vec::new(), Vec::new()),
                },
                _ => (String::new(), Vec::new(), Vec::new()),
            };
            if state.1.is_empty() {
                return Err(
                    vm.throw_builtin("Ljava/lang/IllegalStateException;", "Mac not initialized")
                );
            }
            let out = mac_compute(&state.0, &state.1, &state.2).unwrap_or_default();
            if output_target {
                let offset = i32v(args[2]).max(0) as usize;
                if let V::Ref(array_h) = args[1] {
                    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(array_h) {
                        for (i, byte) in out.iter().enumerate() {
                            if let Some(slot) = data.get_mut(offset + i) {
                                *slot = V::I32(*byte as i8 as i32);
                            }
                        }
                    }
                }
                None
            } else {
                Some(byte_arr(vm, &out))
            }
        }
        "reset" => {
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host {
                    state: HostState::Mac { data, .. },
                    ..
                }) = vm.heap.get_mut(*h)
                {
                    data.clear();
                }
            }
            None
        }
        "getAlgorithm" => {
            let algorithm = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Mac { algorithm, .. },
                        ..
                    }) => algorithm.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            Some(jstr(vm, &algorithm))
        }
        "getMacLength" => {
            let len = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Mac { algorithm, .. },
                        ..
                    }) => mac_output_len(algorithm),
                    _ => 0,
                },
                _ => 0,
            };
            Some(V::I32(len as i32))
        }
        _ => return Ok(None),
    }))
}

fn digest_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    if name == "digest" && args.len() == 4 {
        let needed = match args.first() {
            Some(V::Ref(h)) => match vm.heap.get(*h) {
                Some(Obj::Host {
                    state: HostState::Digest(state),
                    ..
                }) => digest_algorithm(state).1,
                _ => 0,
            },
            _ => 0,
        };
        if i32v(args[3]) < needed {
            return Err(vm.throw_builtin(
                "Ljava/security/DigestException;",
                "insufficient space in digest output buffer",
            ));
        }
    }
    Ok(digest_call_impl(vm, name, args))
}

fn digest_call_impl(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getInstance" => {
            let algorithm = args.first().and_then(|v| sv(vm, *v)).unwrap_or_default();
            let state = digest_new(&algorithm)?;
            Some(Some(host(
                vm,
                "Ljava/security/MessageDigest;",
                HostState::Digest(Box::new(state)),
            )))
        }
        "isEqual" => {
            let a = array_bytes(vm, args.first().copied().unwrap_or(V::Ref(0)), 0, None);
            let b = array_bytes(vm, args.get(1).copied().unwrap_or(V::Ref(0)), 0, None);
            Some(Some(V::I32((a == b) as i32)))
        }
        "update" => {
            let mut buffer_pos = None;
            let bytes = match args.len() {
                2 => match args.get(1).copied().unwrap_or(V::I32(0)) {
                    v @ V::Ref(h) => match vm.heap.get(h) {
                        Some(Obj::Host {
                            state: HostState::Slot(V::Ref(_)),
                            ..
                        }) => {
                            let (bytes, _pos) = bb_bytes(vm, h);
                            if let Some(lim) = pseudo_get(vm, h, P_BBUF_LIM) {
                                buffer_pos = Some((h, i32v(lim)));
                            }
                            bytes
                        }
                        _ => array_bytes(vm, v, 0, None),
                    },
                    v => vec![i32v(v) as u8],
                },
                4 => array_bytes(
                    vm,
                    args.get(1).copied().unwrap_or(V::Ref(0)),
                    i32v(args[2]).max(0) as usize,
                    Some(i32v(args[3]).max(0) as usize),
                ),
                _ => Vec::new(),
            };
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host {
                    state: HostState::Digest(state),
                    ..
                }) = vm.heap.get_mut(*h)
                {
                    digest_update(state, &bytes);
                }
            }
            if let Some((h, pos)) = buffer_pos {
                pseudo_set(vm, h, P_BBUF_POS, V::I32(pos));
            }
            Some(None)
        }
        "digest" => {
            if args.len() == 2 {
                let bytes = array_bytes(vm, args[1], 0, None);
                if let Some(V::Ref(h)) = args.first() {
                    if let Some(Obj::Host {
                        state: HostState::Digest(state),
                        ..
                    }) = vm.heap.get_mut(*h)
                    {
                        digest_update(state, &bytes);
                    }
                }
            }
            let out = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get_mut(*h) {
                    Some(Obj::Host {
                        state: HostState::Digest(state),
                        ..
                    }) => digest_finish(state),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if args.len() == 4 {
                let offset = i32v(args[2]).max(0) as usize;
                let len = i32v(args[3]).max(0) as usize;
                let mut written = 0usize;
                if let V::Ref(array_h) = args[1] {
                    if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(array_h) {
                        for (i, byte) in out.iter().take(len).enumerate() {
                            if let Some(slot) = data.get_mut(offset + i) {
                                *slot = V::I32(*byte as i8 as i32);
                                written += 1;
                            }
                        }
                    }
                }
                return Some(Some(V::I32(written as i32)));
            }
            Some(Some(byte_arr(vm, &out)))
        }
        "reset" => {
            if let Some(V::Ref(h)) = args.first() {
                if let Some(Obj::Host {
                    state: HostState::Digest(state),
                    ..
                }) = vm.heap.get_mut(*h)
                {
                    **state = digest_like(state);
                }
            }
            Some(None)
        }
        "getAlgorithm" => {
            let algorithm = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Digest(state),
                        ..
                    }) => digest_algorithm(state).0,
                    _ => "",
                },
                _ => "",
            };
            Some(Some(jstr(vm, algorithm)))
        }
        "getDigestLength" => {
            let len = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Digest(state),
                        ..
                    }) => digest_algorithm(state).1,
                    _ => 0,
                },
                _ => 0,
            };
            Some(Some(V::I32(len)))
        }
        "toString" => {
            let algorithm = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Digest(state),
                        ..
                    }) => digest_algorithm(state).0,
                    _ => "",
                },
                _ => "",
            };
            Some(Some(jstr(vm, &format!("MessageDigest<{algorithm}>"))))
        }
        _ => None,
    }
}

fn byte_arr(vm: &mut Vm, bytes: &[u8]) -> V {
    arr(
        vm,
        "B",
        bytes.iter().map(|b| V::I32(*b as i8 as i32)).collect(),
    )
}

fn zip_file_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    if name == "<init>" {
        let arg = args.get(1).copied().unwrap_or(V::Ref(0));
        let bytes = stream_arg_path(vm, arg)
            .and_then(|p| std::fs::read(fspath(vm, &p)).ok())
            .unwrap_or_default();
        let entries =
            ovelo_apk::zip::parse_central_directory(&bytes, &ovelo_apk::zip::ZipLimits::default())
                .map(|(_, entries)| entries)
                .unwrap_or_default();
        let cert_der = if class == "Ljava/util/jar/JarFile;" {
            ovelo_apk::sig::verify_v1(&bytes).ok().and_then(|infos| {
                infos
                    .first()
                    .map(|info| std::rc::Rc::new(info.cert_der.clone()))
            })
        } else {
            None
        };
        replace_host_state(
            vm,
            args[0],
            class,
            HostState::ZipArchive {
                bytes: std::rc::Rc::new(bytes),
                entries: std::rc::Rc::new(entries),
                cert_der,
            },
        );
        return Some(None);
    }
    let archive = match args.first() {
        Some(V::Ref(h)) => match vm.heap.get(*h) {
            Some(Obj::Host {
                state:
                    HostState::ZipArchive {
                        bytes,
                        entries,
                        cert_der,
                    },
                ..
            }) => Some((bytes.clone(), entries.clone(), cert_der.clone())),
            _ => None,
        },
        _ => None,
    };
    let Some((bytes, entries, cert_der)) = archive else {
        return Some(Some(stub_ret(vm, "")));
    };
    match name {
        "close" => Some(None),
        "size" => Some(Some(V::I32(entries.len() as i32))),
        "getEntry" | "getJarEntry" => {
            let wanted = args.get(1).and_then(|v| sv(vm, *v)).unwrap_or_default();
            let entry = entries.iter().find(|e| e.name == wanted).cloned();
            let Some(entry) = entry else {
                return Some(Some(V::Ref(0)));
            };
            let desc = if class == "Ljava/util/jar/JarFile;" {
                "Ljava/util/jar/JarEntry;"
            } else {
                "Ljava/util/zip/ZipEntry;"
            };
            Some(Some(host(
                vm,
                desc,
                HostState::ZipEntry {
                    bytes,
                    entry,
                    cert_der,
                },
            )))
        }
        "getInputStream" => Some(Some(zip_input_stream(
            vm,
            args.get(1).copied().unwrap_or(V::Ref(0)),
        ))),
        "entries" => {
            let desc = if class == "Ljava/util/jar/JarFile;" {
                "Ljava/util/jar/JarEntry;"
            } else {
                "Ljava/util/zip/ZipEntry;"
            };
            let items = entries
                .iter()
                .cloned()
                .map(|entry| {
                    host(
                        vm,
                        desc,
                        HostState::ZipEntry {
                            bytes: bytes.clone(),
                            entry,
                            cert_der: cert_der.clone(),
                        },
                    )
                })
                .collect();
            Some(Some(host(
                vm,
                "Ljava/util/Enumeration;",
                HostState::Iter(std::rc::Rc::new(items), 0),
            )))
        }
        "getManifest" => Some(Some(host(vm, "Ljava/util/jar/Manifest;", HostState::Empty))),
        _ => Some(Some(stub_ret(vm, ""))),
    }
}

fn signer_arr(vm: &mut Vm, cert_der: Option<std::rc::Rc<Vec<u8>>>) -> V {
    let Some(der) = cert_der else {
        return arr(vm, "Ljava/security/CodeSigner;", vec![]);
    };
    let cert = host(
        vm,
        "Ljava/security/cert/X509Certificate;",
        HostState::Bytes(der, 0),
    );
    let path = host(
        vm,
        "Ljava/security/cert/CertPath;",
        HostState::List(vec![cert]),
    );
    let signer = host(
        vm,
        "Ljava/security/CodeSigner;",
        HostState::List(vec![path]),
    );
    arr(vm, "Ljava/security/CodeSigner;", vec![signer])
}

fn zip_entry_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    let entry = match args.first() {
        Some(V::Ref(h)) => match vm.heap.get(*h) {
            Some(Obj::Host {
                state:
                    HostState::ZipEntry {
                        bytes,
                        entry,
                        cert_der,
                    },
                ..
            }) => Some((bytes.clone(), entry.clone(), cert_der.clone())),
            _ => None,
        },
        _ => None,
    };
    let Some((bytes, entry, cert_der)) = entry else {
        return Some(Some(stub_ret(vm, "")));
    };
    match name {
        "getName" => Some(Some(jstr(vm, &entry.name))),
        "getSize" => Some(Some(V::I64(entry.uncompressed_size as i64))),
        "getCompressedSize" => Some(Some(V::I64(entry.compressed_size as i64))),
        "getCrc" => Some(Some(V::I64(entry.crc32 as i64))),
        "getMethod" => Some(Some(V::I32(entry.method as i32))),
        "isDirectory" => Some(Some(V::I32(entry.name.ends_with('/') as i32))),
        // The archive signature has already been verified by ovelo-apk;
        // expose an empty attribute map so callers can inspect digest names
        // without treating signed entries as unsigned.
        "getAttributes" => Some(Some(host(
            vm,
            "Ljava/util/HashMap;",
            HostState::Map(vec![]),
        ))),
        "getCodeSigners" => Some(Some(signer_arr(vm, cert_der))),
        "getCertificates" => {
            let cert = cert_der.map(|der| {
                host(
                    vm,
                    "Ljava/security/cert/X509Certificate;",
                    HostState::Bytes(der, 0),
                )
            });
            Some(Some(arr(
                vm,
                "Ljava/security/cert/Certificate;",
                cert.into_iter().collect(),
            )))
        }
        _ => {
            let _ = bytes;
            Some(Some(stub_ret(vm, "")))
        }
    }
}

fn zip_input_stream(vm: &mut Vm, entry_obj: V) -> V {
    let entry = match entry_obj {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::ZipEntry { bytes, entry, .. },
                ..
            }) => Some((bytes.clone(), entry.clone())),
            _ => None,
        },
        _ => None,
    };
    let bytes = entry
        .and_then(|(bytes, entry)| ovelo_apk::zip::read_entry(&bytes, &entry).ok())
        .unwrap_or_default();
    host(
        vm,
        "Ljava/io/ByteArrayInputStream;",
        HostState::Bytes(std::rc::Rc::new(bytes), 0),
    )
}

fn cert_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Option<Option<V>> {
    match (class, name) {
        ("Ljava/security/CodeSigner;", "getSignerCertPath") => match args.first() {
            Some(V::Ref(h)) => match vm.heap.get(*h) {
                Some(Obj::Host {
                    state: HostState::List(items),
                    ..
                }) => Some(Some(items.first().copied().unwrap_or(V::Ref(0)))),
                _ => Some(Some(V::Ref(0))),
            },
            _ => Some(Some(V::Ref(0))),
        },
        ("Ljava/security/cert/CertPath;", "getCertificates") => match args.first() {
            Some(V::Ref(h)) => match vm.heap.get(*h) {
                Some(Obj::Host {
                    state: HostState::List(items),
                    ..
                }) => Some(Some(host(
                    vm,
                    "Ljava/util/ArrayList;",
                    HostState::List(items.clone()),
                ))),
                _ => Some(Some(host(
                    vm,
                    "Ljava/util/ArrayList;",
                    HostState::List(vec![]),
                ))),
            },
            _ => Some(Some(host(
                vm,
                "Ljava/util/ArrayList;",
                HostState::List(vec![]),
            ))),
        },
        (_, "getEncoded") => {
            let bytes = match args.first() {
                Some(V::Ref(h)) => match vm.heap.get(*h) {
                    Some(Obj::Host {
                        state: HostState::Bytes(bytes, pos),
                        ..
                    }) => bytes[*pos..].to_vec(),
                    _ => vec![],
                },
                _ => vec![],
            };
            Some(Some(byte_arr(vm, &bytes)))
        }
        (_, "getType") => Some(Some(jstr(vm, "X.509"))),
        _ => Some(Some(stub_ret(vm, ""))),
    }
}

/// The guest path a stream ctor argument points at: `File` (Path host),
/// `String`, or `Uri`/`file:` string. None for other objects.
fn stream_arg_path(vm: &Vm, v: V) -> Option<String> {
    let V::Ref(h) = v else { return None };
    let raw = match vm.heap.get(h) {
        Some(Obj::Str(s)) => s.clone(),
        Some(Obj::Host {
            state: HostState::Path(s) | HostState::StrBuf(s) | HostState::Regex(s),
            ..
        }) => s.clone(),
        _ => return None,
    };
    Some(raw.strip_prefix("file://").unwrap_or(&raw).to_string())
}
