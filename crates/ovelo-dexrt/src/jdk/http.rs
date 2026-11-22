//! okhttp3 `RealCall.getResponseWithInterceptorChain$okhttp`
//! interception — real HTTP(S) over the host's TLS stack
//! (ureq/rustls). Every call path (`Call.execute`, the dispatcher's
//! `AsyncCall.run`, and helpers like coil's `await`) funnels through
//! that one method, so intercepting it covers them all and the app's
//! own code delivers callbacks — no done-runnables needed. The method
//! parks the runnable while a host thread performs the request; on
//! re-run the finished response is synthesized through the app's own
//! `Response.Builder`, `Headers.of` and `ResponseBody.create` —
//! downstream guest code sees ordinary okhttp objects.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::*;
use crate::value::{Obj, V};
use crate::{errd, E, InvokeKind, GX_NONE};
use ovelo_error::Result;

/// Ticket pseudo field — set on the RealCall while a fetch is in
/// flight so a re-run finds the request instead of starting another.
const P_HTTP: u64 = u32::MAX as u64 - 45;

struct HttpSpec {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct HttpResp {
    code: u16,
    message: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct HttpSlot {
    started: Instant,
    result: Option<std::result::Result<HttpResp, String>>,
}

static HTTP_NEXT: AtomicU64 = AtomicU64::new(1);
static HTTP_SLOTS: OnceLock<Mutex<HashMap<u64, HttpSlot>>> = OnceLock::new();

fn slots() -> &'static Mutex<HashMap<u64, HttpSlot>> {
    HTTP_SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .new_agent()
    })
}

fn do_fetch(spec: HttpSpec) -> std::result::Result<HttpResp, String> {
    let a = agent();
    // Build through `http::Request` so arbitrary methods (DELETE with a
    // body, PROPFIND, ...) keep their verb — the per-method Agent
    // helpers silently remap anything unlisted to GET.
    let mut req = ureq::http::Request::builder()
        .method(spec.method.as_str())
        .uri(spec.url.as_str());
    for (k, v) in &spec.headers {
        if !k.eq_ignore_ascii_case("accept-encoding") {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    let mut resp = if spec.body.is_empty()
        && !matches!(spec.method.as_str(), "POST" | "PUT" | "PATCH")
    {
        let req = req.body(ureq::SendBody::none()).map_err(|e| e.to_string())?;
        a.run(req).map_err(|e| e.to_string())?
    } else {
        // An empty body vec still forces Content-Length: 0 — the write
        // methods expect it.
        let req = req.body(spec.body.clone()).map_err(|e| e.to_string())?;
        a.run(req).map_err(|e| e.to_string())?
    };
    let code = resp.status().as_u16();
    let message = resp
        .status()
        .canonical_reason()
        .unwrap_or_default()
        .to_string();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let body = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok(HttpResp {
        code,
        message,
        headers,
        body,
    })
}

fn spawn_fetch(spec: HttpSpec) -> u64 {
    let ticket = HTTP_NEXT.fetch_add(1, Ordering::Relaxed);
    slots().lock().unwrap().insert(
        ticket,
        HttpSlot {
            started: Instant::now(),
            result: None,
        },
    );
    std::thread::spawn(move || {
        let r = do_fetch(spec);
        if httpdbg() {
            match &r {
                Ok(resp) => eprintln!(
                    "[http] done ticket={ticket} code={} bytes={}",
                    resp.code,
                    resp.body.len()
                ),
                Err(e) => eprintln!("[http] done ticket={ticket} err={e}"),
            }
        }
        if let Some(s) = slots().lock().unwrap().get_mut(&ticket) {
            s.result = Some(r);
        }
    });
    ticket
}

/// Finished result for `ticket` — `None` while in flight, `Err` past
/// the 60s cap even if the fetch thread is still blocked.
fn take_result(ticket: u64) -> Option<std::result::Result<HttpResp, String>> {
    let mut m = slots().lock().unwrap();
    match m.get(&ticket) {
        Some(s) if s.result.is_some() => m.remove(&ticket).and_then(|s| s.result),
        Some(s) if s.started.elapsed() > Duration::from_secs(60) => {
            m.remove(&ticket);
            Some(Err("request timeout".into()))
        }
        _ => None,
    }
}

/// Wait for the host-side fetch to land. `Call.execute()` is
/// synchronous in OkHttp — block in place so the caller's continuation
/// runs exactly once. Parking-and-re-running the runnable replays its
/// already-completed side effects (e.g. rxjava `onSubscribe` →
/// `DisposableHelper.setOnce` fails on the second pass with
/// `ProtocolViolationException`). The fetch itself runs on a host
/// thread, so blocking the scheduler here only delays peer runnables
/// for the request's duration — the same stall a real blocking
/// `execute` imposes on its thread.
fn park_or_block(vm: &mut Vm, call_h: u32, ticket: u64) -> Result<Option<V>> {
    let deadline = now_ms() + 30_000;
    loop {
        if let Some(r) = take_result(ticket) {
            if httpdbg() {
                eprintln!("[http] delivered ticket={ticket}");
            }
            let req = request_of(vm, call_h).unwrap_or(0);
            return match r {
                Ok(resp) => build_response(vm, req, resp).map(|h| Some(V::Ref(h))),
                Err(e) => Ok(Some(V::Ref(fake_failure(vm, req, &e)?))),
            };
        }
        if now_ms() > deadline {
            if httpdbg() {
                eprintln!("[http] deadline ticket={ticket}");
            }
            // Drop the slot too — `take_result` is never called for this
            // ticket again, so the entry would sit in the map forever.
            slots().lock().unwrap().remove(&ticket);
            let req = request_of(vm, call_h).unwrap_or(0);
            return Ok(Some(V::Ref(fake_failure(vm, req, "timeout")?)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read the `Request` out of a RealCall — `Call.request()` first, the
/// `originalRequest` field as the fallback for okhttp versions that
/// keep it under a different accessor name.
fn request_of(vm: &mut Vm, call_h: u32) -> Result<u32> {
    if let Ok(Some(V::Ref(h))) = vm.call_virtual(call_h, "request", &[]) {
        if h != 0 {
            return Ok(h);
        }
    }
    let ci = vm.heap.class_of(call_h).unwrap_or(GX_NONE);
    if let Some(f) = vm.find_field_by_name(ci, "originalRequest", false, None) {
        if let Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) =
            vm.heap.get(call_h)
        {
            if let Some(V::Ref(h)) = fields.get(&f) {
                if *h != 0 {
                    return Ok(*h);
                }
            }
        }
    }
    if httpdbg() {
        let cls = vm.type_desc_of(ci).unwrap_or_else(|_| "?".into());
        let f = vm.find_field_by_name(ci, "originalRequest", false, None);
        let shape = match vm.heap.get(call_h) {
            Some(Obj::Instance { .. }) => "instance",
            Some(Obj::Host { .. }) => "host",
            _ => "other",
        };
        eprintln!("[http] no_request h={call_h} class={cls} shape={shape} field={f:?}");
    }
    Err(errd("dexrt.http.no_request", "".into()))
}

/// Read an instance field by name (OkHttp 5 exposes `url`/`method`/
/// `headers`/`body` as public fields instead of getters).
fn field_v(vm: &Vm, h: u32, name: &str, want: Option<&str>) -> Option<V> {
    let ci = vm.heap.class_of(h)?;
    let f = vm.find_field_by_name(ci, name, false, want)?;
    match vm.heap.get(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.get(&f).copied()
        }
        _ => None,
    }
}

/// OkHttp 4 (`request.url()`) or OkHttp 5 (`request.url` field) —
/// try the getter first, then the field.
fn member_ref(vm: &mut Vm, h: u32, name: &str, want: Option<&str>) -> Option<u32> {
    match vm.call_virtual(h, name, &[]) {
        Ok(Some(V::Ref(r))) if r != 0 => Some(r),
        _ => match field_v(vm, h, name, want) {
            Some(V::Ref(r)) if r != 0 => Some(r),
            _ => None,
        },
    }
}

fn member_str(vm: &mut Vm, h: u32, name: &str) -> Option<String> {
    match vm.call_virtual(h, name, &[]) {
        Ok(Some(v)) => sv(vm, v),
        _ => field_v(vm, h, name, Some("Ljava/lang/String;")).and_then(|v| sv(vm, v)),
    }
}

fn request_spec(vm: &mut Vm, call_h: u32) -> Result<HttpSpec> {
    let req = request_of(vm, call_h)?;
    let url = match member_ref(vm, req, "url", Some("Lokhttp3/HttpUrl;")) {
        Some(h) => {
            // HttpUrl.toString() just returns its `url` String field —
            // read it directly so a busted toString path can't lose the
            // value.
            field_v(vm, h, "url", Some("Ljava/lang/String;"))
                .and_then(|v| sv(vm, v))
                .or_else(|| member_str(vm, h, "toString"))
                .unwrap_or_default()
        }
        None => String::new(),
    };
    let method =
        member_str(vm, req, "method").unwrap_or_else(|| "GET".into());
    let mut headers = Vec::new();
    if let Some(hh) = member_ref(vm, req, "headers", Some("Lokhttp3/Headers;"))
    {
        // OkHttp 5: flat `namesAndValues` String[] field, name/value pairs.
        // OkHttp 4: `size()`/`name(i)`/`value(i)` methods.
        let pairs = match field_v(vm, hh, "namesAndValues", Some("[Ljava/lang/String;"))
        {
            Some(V::Ref(arr)) => match vm.heap.get(arr) {
                Some(Obj::Array { data, .. }) => data
                    .iter()
                    .map(|v| sv(vm, *v).unwrap_or_default())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if !pairs.is_empty() {
            for w in pairs.chunks(2) {
                let k = &w[0];
                let v = w.get(1).map(|s| s.as_str()).unwrap_or_default();
                if !k.eq_ignore_ascii_case("content-length") && !k.is_empty() {
                    headers.push((k.clone(), v.to_string()));
                }
            }
        } else {
            let n = match vm.call_virtual(hh, "size", &[])? {
                Some(V::I32(n)) => n.max(0) as usize,
                _ => 0,
            };
            for i in 0..n {
                let k = vm
                    .call_virtual(hh, "name", &[V::I32(i as i32)])?
                    .and_then(|v| sv(vm, v))
                    .unwrap_or_default();
                let v = vm
                    .call_virtual(hh, "value", &[V::I32(i as i32)])?
                    .and_then(|v| sv(vm, v))
                    .unwrap_or_default();
                // The host stack sets these itself — passing them
                // through would double-send framing headers.
                if !k.eq_ignore_ascii_case("content-length") && !k.is_empty() {
                    headers.push((k, v));
                }
            }
        }
    }
    // POST/PUT/PATCH body: run `body.writeTo(buffer)` against the
    // app's own okio.Buffer, then read the bytes back out.
    let mut body = Vec::new();
    if let Some(bh) = member_ref(vm, req, "body", None) {
        // `contentType()` returns a MediaType — read its `mediaType`
        // field, or stringify when the field layout differs.
        if let Ok(Some(V::Ref(mh))) = vm.call_virtual(bh, "contentType", &[]) {
            let ct = member_str(vm, mh, "mediaType").or_else(|| {
                vm.call_virtual(mh, "toString", &[])
                    .ok()
                    .flatten()
                    .and_then(|v| sv(vm, v))
            });
            // OkHttp keeps an explicit Content-Type header over the
            // body's own media type — don't append a second one.
            if let Some(ct) = ct {
                if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                    headers.push(("Content-Type".into(), ct));
                }
            }
        }
        if let Ok(buf) = vm.instantiate("Lokio/Buffer;") {
            // OkHttp 5's `toRequestBody` bodies take a `RealBufferedSink`
            // and write into its `bufferField` — a bare `Buffer` leaves
            // that field null. Wrap ours; the ctor accepts any `Sink`.
            let sink = vm
                .instantiate_with("Lokio/RealBufferedSink;", &[V::Ref(buf)])
                .unwrap_or(buf);
            let w = vm.call_virtual(bh, "writeTo", &[V::Ref(sink)]);
            if httpdbg() {
                eprintln!("[http] writeTo buf={buf} sink={sink} ok={:?}", w.is_ok());
            }
            // A failed writeTo must fail the request — sending the empty
            // body silently corrupts every mutating call.
            w?;
            // `flush()` pushes chunks to our downstream `buf`; the
            // unflushed tail stays in the sink's `bufferField`. The
            // flushed bytes come first, so drain buf then bufferField.
            let drain = member_ref(vm, sink, "bufferField", None).unwrap_or(buf);
            body = drain_buffer(vm, buf);
            body.extend(drain_buffer(vm, drain));
        }
    }
    Ok(HttpSpec {
        method,
        url,
        headers,
        body,
    })
}

/// Read all bytes out of an okio `Buffer` — this okio only exposes
/// `readByteArray(long)`, so pull the size field ourselves.
fn drain_buffer(vm: &mut Vm, h: u32) -> Vec<u8> {
    let n = match field_v(vm, h, "size", Some("J")) {
        Some(v) => i64v(v),
        _ => match vm.call_virtual(h, "size", &[]) {
            Ok(Some(v)) => i64v(v),
            _ => 0,
        },
    };
    if n <= 0 {
        return Vec::new();
    }
    match vm.call_virtual_sig(h, "readByteArray", &["J"], &[V::I64(n)]) {
        Ok(Some(V::Ref(arr))) => match vm.heap.get(arr) {
            Some(Obj::Array { data, .. }) => data.iter().map(|v| i64v(*v) as u8).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Invoke a static by exact parameter list (`name` is shared across
/// overloads — `Headers.of(Map)` vs `of(String...)`).
fn static_sig(
    vm: &mut Vm,
    owner: &str,
    name: &str,
    params: &[&str],
    args: Vec<V>,
) -> Result<Option<V>> {
    let ci = vm.type_index_of(owner).unwrap_or(GX_NONE);
    let Some(midx) = vm.find_method_on_chain_sig(ci, name, params)? else {
        return Ok(None);
    };
    match vm.invoke_pub(midx, args, InvokeKind::Static) {
        Ok(v) => Ok(v),
        Err(E::Vm(e)) => Err(e),
        Err(E::Throw(t)) => {
            let d = vm.describe_throw(t);
            Err(errd("dexrt.exception.uncaught", d))
        }
    }
}

/// `ResponseBody.create(MediaType?, byte[])` — the `@JvmStatic`
/// deprecated companion facade okhttp keeps for Java callers. OkHttp 5's
/// R8 build drops it; fall back to `RealResponseBody(String, long,
/// RealBufferedSource)` over a byte-backed `okio.Buffer`.
fn response_body(vm: &mut Vm, content_type: &str, bytes: &[u8]) -> Result<u32> {
    let arr = vm.heap.alloc(Obj::Array {
        elem: "B".into(),
        data: bytes.iter().map(|b| V::I32(*b as i8 as i32)).collect(),
    });
    let mt = if content_type.is_empty() {
        V::Ref(0)
    } else {
        {
            let s = jstr(vm, content_type);
            static_sig(vm, "Lokhttp3/MediaType;", "get", &["Ljava/lang/String;"], vec![s])?
        }
        .unwrap_or(V::Ref(0))
    };
    if let Some(V::Ref(h)) = static_sig(
        vm,
        "Lokhttp3/ResponseBody;",
        "create",
        &["Lokhttp3/MediaType;", "[B"],
        vec![mt, V::Ref(arr)],
    )? {
        return Ok(h);
    }
    if vm.type_index_of("Lokhttp3/internal/http/RealResponseBody;").is_none() {
        return Ok(0);
    }
    let buf = vm.instantiate("Lokio/Buffer;")?;
    let bs = vm.instantiate_with("Lokio/ByteString;", &[V::Ref(arr)])?;
    vm.call_virtual_sig(buf, "write", &["Lokio/ByteString;"], &[V::Ref(bs)])?;
    let src = vm.instantiate_with("Lokio/RealBufferedSource;", &[V::Ref(buf)])?;
    let ct = if content_type.is_empty() {
        V::Ref(0)
    } else {
        jstr(vm, content_type)
    };
    vm.instantiate_with(
        "Lokhttp3/internal/http/RealResponseBody;",
        &[ct, V::I64(bytes.len() as i64), V::Ref(src)],
    )
}

/// A Response the guest can read `code()`/`message()`/`body()` off,
/// assembled from the wire result through okhttp's own Builder.
fn build_response(vm: &mut Vm, req_h: u32, resp: HttpResp) -> Result<u32> {
    const BLD: &str = "Lokhttp3/Response$Builder;";
    let b = vm.instantiate(BLD)?;
    // Protocol.HTTP_2 enum constant.
    let proto = vm
        .type_index_of("Lokhttp3/Protocol;")
        .and_then(|ci| vm.find_field_by_name(ci, "HTTP_2", true, None))
        .and_then(|f| vm.sget_pub(f))
        .unwrap_or(V::Ref(0));
    let ct = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let body = response_body(vm, &ct, &resp.body)?;

    // OkHttp 5 rewrote Response.Builder as public fields; OkHttp 4
    // keeps setter methods. Pick by probing for the `code` field.
    let field_mode = vm
        .type_index_of(BLD)
        .and_then(|ci| vm.find_field_by_name(ci, "code", false, Some("I")))
        .is_some();
    if field_mode {
        if req_h != 0 {
            vm.set_object_field(b, BLD, "request", V::Ref(req_h));
        }
        vm.set_object_field(b, BLD, "protocol", proto);
        vm.set_object_field(b, BLD, "code", V::I32(resp.code as i32));
        let m = jstr(vm, &resp.message);
        vm.set_object_field(b, BLD, "message", m);
        // `headers` is a Headers.Builder whose `namesAndValues` is a
        // flat ArrayList<String>. The class's only ctors take the list
        // or a dummy byte — instantiate skips a missing `<init>()`, so
        // allocate it and store the populated list directly.
        let hb = vm.instantiate("Lokhttp3/Headers$Builder;")?;
        let flat: Vec<V> = resp
            .headers
            .iter()
            .flat_map(|(k, v)| [jstr(vm, k), jstr(vm, v)])
            .collect();
        let list = host(vm, "Ljava/util/ArrayList;", HostState::List(flat));
        vm.set_object_field(hb, "Lokhttp3/Headers$Builder;", "namesAndValues", list);
        vm.set_object_field(b, BLD, "headers", V::Ref(hb));
        if body != 0 {
            vm.set_object_field(b, BLD, "body", V::Ref(body));
        }
        let now = V::I64(now_ms());
        vm.set_object_field(b, BLD, "sentRequestAtMillis", now);
        vm.set_object_field(b, BLD, "receivedResponseAtMillis", now);
    } else {
        if req_h != 0 {
            vm.call_virtual(b, "request", &[V::Ref(req_h)])?;
        }
        vm.call_virtual(b, "protocol", &[proto])?;
        vm.call_virtual(b, "code", &[V::I32(resp.code as i32)])?;
        {
            let m = jstr(vm, &resp.message);
            vm.call_virtual(b, "message", &[m])?;
        }
        // Headers.of(vararg String).
        let flat: Vec<V> = resp
            .headers
            .iter()
            .flat_map(|(k, v)| [jstr(vm, k), jstr(vm, v)])
            .collect();
        let arr = vm.heap.alloc(Obj::Array {
            elem: "Ljava/lang/String;".into(),
            data: flat,
        });
        if let Some(hdrs) = static_sig(
            vm,
            "Lokhttp3/Headers;",
            "of",
            &["[Ljava/lang/String;"],
            vec![V::Ref(arr)],
        )? {
            vm.call_virtual(b, "headers", &[hdrs])?;
        }
        if body != 0 {
            vm.call_virtual(b, "body", &[V::Ref(body)])?;
        }
    }
    match vm.call_virtual(b, "build", &[])? {
        Some(V::Ref(h)) => Ok(h),
        _ => Err(errd("dexrt.http.build", "".into())),
    }
}

/// Transport failure inside `execute` — host fns can't raise a guest
/// exception, so answer with a response whose non-2xx code the app's
/// own error path handles (NewPipe checks `isSuccessful`).
fn fake_failure(vm: &mut Vm, req_h: u32, why: &str) -> Result<u32> {
    build_response(
        vm,
        req_h,
        HttpResp {
            code: 599,
            message: format!("Connect error: {why}"),
            headers: Vec::new(),
            body: Vec::new(),
        },
    )
}

fn httpdbg() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var_os("OVELO_HTTPDBG").is_some())
}

fn do_execute(vm: &mut Vm, call_h: u32) -> Result<Option<V>> {
    if httpdbg() {
        eprintln!("[http] execute h={call_h}");
    }
    match pseudo_get(vm, call_h, P_HTTP) {
        Some(V::I64(ticket)) => match take_result(ticket as u64) {
            None => park_or_block(vm, call_h, ticket as u64),
            Some(Ok(resp)) => {
                pseudo_set(vm, call_h, P_HTTP, V::I32(0));
                let req = request_of(vm, call_h).unwrap_or(0);
                build_response(vm, req, resp).map(|h| Some(V::Ref(h)))
            }
            Some(Err(e)) => {
                pseudo_set(vm, call_h, P_HTTP, V::I32(0));
                let req = request_of(vm, call_h).unwrap_or(0);
                Ok(Some(V::Ref(fake_failure(vm, req, &e)?)))
            }
        },
        _ => {
            let spec = request_spec(vm, call_h)?;
            if httpdbg() {
                eprintln!("[http] fetch {} {}", spec.method, spec.url);
                for (k, v) in &spec.headers {
                    eprintln!("[http]   {k}: {v}");
                }
                if !spec.body.is_empty() {
                    let n = spec.body.len().min(600);
                    eprintln!(
                        "[http]   body {}B: {}",
                        spec.body.len(),
                        String::from_utf8_lossy(&spec.body[..n])
                    );
                }
            }
            let ticket = spawn_fetch(spec);
            pseudo_set(vm, call_h, P_HTTP, V::I64(ticket as i64));
            park_or_block(vm, call_h, ticket)
        }
    }
}

/// Called from `Vm::add_dex` — intercept the single method every
/// okhttp call path funnels through (`execute`, `enqueue` via
/// `RealCall$AsyncCall.run`, coil's `await`, Rx wrappers).
pub(crate) fn install(vm: &mut Vm) {
    for class in [
        "Lokhttp3/internal/connection/RealCall;",
        "Lokhttp3/RealCall;",
    ] {
        let ok = vm.register_host(
            class,
            "getResponseWithInterceptorChain$okhttp",
            "()Lokhttp3/Response;",
            std::rc::Rc::new(std::cell::RefCell::new(|vm: &mut Vm, a: &[V]| {
                let this = match a.first() {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                };
                do_execute(vm, this)
            })),
        );
        if httpdbg() {
            eprintln!("[http] registered {class} interceptorChain={ok:?}");
        }
    }
    // OkHttp 5's `toCanonicalHost` runs a bundled punycode/IDNA dex
    // pipeline; a miss anywhere in it rejects every host with
    // "Invalid URL host". ASCII hosts only need lowercase + pass
    // through, which is all NewPipe et al. hit in practice.
    let class = "Lokhttp3/internal/_HostnamesCommonKt;";
    let ok = vm.register_host(
        class,
        "toCanonicalHost",
        "(Ljava/lang/String;)Ljava/lang/String;",
        std::rc::Rc::new(std::cell::RefCell::new(|vm: &mut Vm, a: &[V]| {
            let t = sv(vm, a.first().copied().unwrap_or(V::Ref(0)))
                .unwrap_or_default();
            Ok(Some(jstr(vm, &t.to_lowercase())))
        })),
    );
    if httpdbg() {
        eprintln!("[http] registered {class} toCanonicalHost={ok:?}");
    }
}
