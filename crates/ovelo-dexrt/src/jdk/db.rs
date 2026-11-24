//! `android.database.sqlite` backed by real SQLite (rusqlite).
//!
//! A `SQLiteDatabase` handle is bound to a database name; connections
//! open lazily per name under the guest sandbox (`databases/`).
//! Queries materialize into a `Cursor` host object whose columns/rows/
//! position live in pseudo fields — androidx Room + SupportSQLite* walk
//! it through the standard `Cursor` API.

use std::cell::RefCell;
use std::rc::Rc;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{params_from_iter, types::Value as SV, types::ValueRef, Connection};

use super::io::fspath;
use super::*;
use crate::value::{HostState, Obj, V};
use crate::{Rt, Vm, E};

// Pseudo field-idx keys for cursor/statement/db plumbing.
pub(crate) const P_CUR_COLS: u32 = u32::MAX - 30;
pub(crate) const P_CUR_ROWS: u32 = u32::MAX - 31;
pub(crate) const P_CUR_POS: u32 = u32::MAX - 32;
pub(crate) const P_SQL: u32 = u32::MAX - 33;
pub(crate) const P_DBNAME: u32 = u32::MAX - 34;
pub(crate) const P_DBVER: u32 = u32::MAX - 35;

/// One open SQLite database (connection + transaction state).
pub(crate) struct Db {
    pub conn: Connection,
    /// Per-level success flags — one bool per open `beginTransaction`,
    /// so a nested `endTransaction` can never commit the outer level.
    pub tx_stack: Vec<bool>,
    /// A nested level ended without `setTransactionSuccessful` — the
    /// whole transaction is doomed; the outermost end rolls back even
    /// if the outer level is later marked successful.
    pub tx_failed: bool,
}

fn db_dir(vm: &Vm) -> std::path::PathBuf {
    fspath(vm, "/data/local/ovelo/databases")
}

/// File name a guest database name maps to inside the sandbox dir —
/// strips every path component so `../` or absolute paths handed to
/// `SQLiteOpenHelper`/`openOrCreateDatabase` can't escape it.
fn safe_name(name: &str) -> String {
    if name == ":memory:" {
        return name.to_string();
    }
    let n = std::path::Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if n.is_empty() || n == ".." {
        "ovelo".to_string()
    } else {
        n
    }
}

/// Open (or fetch) the pooled connection for `name`; `:memory:` stays
/// in RAM. Callers that need an independent handle (the bundled
/// androidx driver — Room puts reader connections under
/// `PRAGMA query_only`, which must not poison the writer) use
/// `open_raw` instead.
fn conn_for(vm: &mut Vm, name: &str) -> Result<Rc<RefCell<Db>>, E> {
    let name = safe_name(name);
    if let Some(d) = vm.dbs.get(&name) {
        return Ok(d.clone());
    }
    let d = open_raw(vm, &name)?;
    vm.dbs.insert(name, d.clone());
    Ok(d)
}

/// A fresh connection to `name` — same sandboxing, collations and
/// authorizer as the pooled path, but not shared with `vm.dbs`.
fn open_raw(vm: &mut Vm, name: &str) -> Result<Rc<RefCell<Db>>, E> {
    let conn = if name == ":memory:" {
        Connection::open_in_memory()
    } else {
        let dir = db_dir(vm);
        let _ = std::fs::create_dir_all(&dir);
        Connection::open(dir.join(name))
    }
    .map_err(|e| {
        vm.throw_builtin(
            "Landroid/database/sqlite/SQLiteException;",
            &format!("open {name}: {e}"),
        )
    })?;
    // Guest SQL reaches the host engine as text — ATTACH DATABASE with
    // an absolute path would escape the sandbox, so deny it (and
    // DETACH) at the SQLite authorizer.
    conn.authorizer(Some(|ctx: AuthContext| match ctx.action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    // Android extends SQLite's built-ins (BINARY/NOCASE/RTRIM) with
    // UNICODE (ICU, locale-independent) and LOCALIZED (locale-aware)
    // collations — Room emits `COLLATE UNICODE` in its queries.
    for name in ["UNICODE", "LOCALIZED"] {
        let _ = conn.create_collation(name, |a, b| {
            // Case-fold + canonical-decomposition compare: without NFD,
            // canonically-equivalent `é` vs `e\u0301` sort as different
            // keys — equality lookups miss and UNIQUE columns accept
            // duplicates ICU would reject.
            use unicode_normalization::UnicodeNormalization;
            let an: String = a.nfd().flat_map(char::to_lowercase).collect();
            let bn: String = b.nfd().flat_map(char::to_lowercase).collect();
            an.cmp(&bn)
        });
    }
    Ok(Rc::new(RefCell::new(Db {
        conn,
        tx_stack: Vec::new(),
        tx_failed: false,
    })))
}

/// The `SQLiteDatabase` object for `name` — one per name (AOSP caches
/// database connections per thread; one shared handle is close enough).
fn db_obj(vm: &mut Vm, name: &str) -> Result<V, E> {
    let name = safe_name(name);
    conn_for(vm, &name)?;
    if let Some(&h) = vm.db_handles.get(&name) {
        return Ok(V::Ref(h));
    }
    let V::Ref(h) = host(
        vm,
        "Landroid/database/sqlite/SQLiteDatabase;",
        HostState::Empty,
    ) else {
        unreachable!()
    };
    vm.db_handles.insert(name.clone(), h);
    vm.obj_db.insert(h, name);
    Ok(V::Ref(h))
}

/// `Context.openOrCreateDatabase(name, mode, …)` and
/// `SQLiteDatabase.openOrCreateDatabase(path|File)` land here.
pub(crate) fn open_or_create(vm: &mut Vm, name: &str) -> Result<V, E> {
    // A full path keeps only its file name inside the sandboxed dir.
    db_obj(vm, &safe_name(name))
}

fn db_name_of(vm: &Vm, h: u32) -> Option<String> {
    vm.obj_db
        .get(&h)
        .cloned()
        .or_else(|| pseudo_get(vm, h, P_DBNAME).and_then(|v| sv(vm, v)))
}

/// Record the db name a helper/db object refers to —
/// `SQLiteOpenHelper.<init>` stores it on the receiver.
fn bind_name(vm: &mut Vm, h: u32, name: &str) {
    let v = jstr(vm, name);
    pseudo_set(vm, h, P_DBNAME, v);
}

fn conn_of(vm: &mut Vm, h: u32) -> Result<Rc<RefCell<Db>>, E> {
    let name = db_name_of(vm, h).unwrap_or_else(|| "ovelo".to_string());
    conn_for(vm, &name)
}

fn sql_err(vm: &mut Vm, e: impl std::fmt::Display) -> E {
    vm.throw_builtin("Landroid/database/sqlite/SQLiteException;", &e.to_string())
}

/// End the outermost transaction level. A failed COMMIT (deferred
/// FK violation, …) leaves the SQLite transaction open, so roll it
/// back — otherwise every later beginTransaction fails too — and
/// still report the error so callers don't treat it as committed.
fn commit_or_rollback(conn: &mut rusqlite::Connection, ok: bool) -> rusqlite::Result<()> {
    match conn.execute_batch(if ok { "COMMIT" } else { "ROLLBACK" }) {
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
        r => r,
    }
}

fn v_to_sql(vm: &Vm, v: V) -> SV {
    match v {
        V::I32(i) => SV::Integer(i as i64),
        V::I64(i) => SV::Integer(i),
        V::F32(f) => SV::Real(f as f64),
        V::F64(f) => SV::Real(f),
        V::Ref(0) => SV::Null,
        V::Ref(h) => {
            if let Some(t) = s(vm, h) {
                SV::Text(t)
            } else if let Some(Obj::Array { data, .. }) = vm.heap.get(h) {
                SV::Blob(data.iter().map(|b| i32v(*b) as u8).collect())
            } else if let Some(Obj::Host {
                state: HostState::Slot(inner),
                ..
            }) = vm.heap.get(h)
            {
                // Boxed Integer/Long/Double/Boolean — bind the payload.
                v_to_sql(vm, *inner)
            } else {
                SV::Null
            }
        }
    }
}

/// Guest `String[] selectionArgs` / `Object[]` → rusqlite params.
fn binds_of(vm: &Vm, v: V) -> Vec<SV> {
    let V::Ref(h) = v else { return Vec::new() };
    match vm.heap.get(h) {
        Some(Obj::Array { data, .. }) => data.iter().map(|e| v_to_sql(vm, *e)).collect(),
        Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) => l.iter().map(|e| v_to_sql(vm, *e)).collect(),
        _ => Vec::new(),
    }
}

fn sqlv_to_v(vm: &mut Vm, c: SV) -> V {
    match c {
        SV::Null => V::Ref(0),
        SV::Integer(i) => V::I64(i),
        SV::Real(f) => V::F64(f),
        SV::Text(t) => jstr(vm, &t),
        SV::Blob(b) => arr(vm, "B", b.iter().map(|&x| V::I32(x as i32)).collect()),
    }
}

/// Run `sql` with `binds`, materializing every row into a Cursor.
fn run_query(vm: &mut Vm, h: u32, sql: &str, binds: V) -> Result<V, E> {
    let params = binds_of(vm, binds);
    let db = conn_of(vm, h)?;
    let (cols, cells): (Vec<String>, Vec<Vec<SV>>) = {
        let d = db.borrow();
        let mut stmt = d.conn.prepare(sql).map_err(|e| sql_err(vm, e))?;
        let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = stmt
            .query(params_from_iter(params))
            .map_err(|e| sql_err(vm, e))?;
        let mut cells = Vec::new();
        while let Some(r) = rows.next().map_err(|e| sql_err(vm, e))? {
            let mut row = Vec::with_capacity(cols.len());
            for i in 0..cols.len() {
                let v = r.get_ref(i).map_err(|e| sql_err(vm, e))?;
                row.push(sv_ref(v));
            }
            cells.push(row);
        }
        (cols, cells)
    };
    let col_v: Vec<V> = cols.iter().map(|c| jstr(vm, c)).collect();
    let col_arr = arr(vm, STRING, col_v);
    let mut row_refs = Vec::with_capacity(cells.len());
    for row in cells {
        let vals: Vec<V> = row.into_iter().map(|c| sqlv_to_v(vm, c)).collect();
        row_refs.push(arr(vm, OBJ, vals));
    }
    let rows_l = host(vm, "Ljava/util/ArrayList;", HostState::List(row_refs));
    let V::Ref(c) = host(vm, "Landroid/database/Cursor;", HostState::Empty) else {
        unreachable!()
    };
    pseudo_set(vm, c, P_CUR_COLS, col_arr);
    pseudo_set(vm, c, P_CUR_ROWS, rows_l);
    pseudo_set(vm, c, P_CUR_POS, V::I32(-1));
    Ok(V::Ref(c))
}

fn first_string(vm: &Vm, args: &[V]) -> Option<String> {
    args.iter().find_map(|a| sv(vm, *a))
}

/// Java `String.hashCode` — MediaStore's `bucket_id` is the lowercase
/// parent path's hashCode, and a stable per-file id keeps `_id`
/// consistent across filtered projections.
fn java_hash(s: &str) -> i32 {
    s.chars()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

/// `ContentResolver.query(content://media/...)` — scan `external_root`
/// into a Cursor carrying the MediaStore column set. `external/images/
/// media`, `external/images/thumbnails` (same rows), `external/file`
/// and friends all share the on-disk scan; a `selection`/`selectionArgs`
/// narrowing isn't applied. `projection` is the caller's requested
/// column list — positional reads (`getString(0)`) must return the
/// requested column, not the full-schema first column.
pub(crate) fn media_cursor(vm: &mut Vm, uri: &str, projection: &[String]) -> V {
    const COLS: &[&str] = &[
        "_id",
        "_data",
        "_display_name",
        "title",
        "_size",
        "date_added",
        "date_modified",
        "datetaken",
        "mime_type",
        "media_type",
        "bucket_id",
        "bucket_display_name",
        "relative_path",
        "width",
        "height",
        "orientation",
        "duration",
        "is_favorite",
        "volume_name",
        "artist",
        "album",
        "album_id",
        "artist_id",
        "track",
        "year",
        "genre_id",
        "genre",
        "composer",
        "is_music",
        "is_ringtone",
        "is_notification",
        "is_alarm",
        "is_audiobook",
        "bookmark",
        // MediaStore.Audio aggregate columns — artists/albums/genres
        // content URIs feed these; per-file URIs leave them 0/"".
        "number_of_tracks",
        "number_of_albums",
        "numsongs",
        "minyear",
        "name",
        "disc_number",
    ];
    let images = uri.contains("/images/");
    let video = uri.contains("/video/");
    let audio = uri.contains("/audio/");
    let mut rows: Vec<Vec<V>> = Vec::new();
    let mut files: Vec<(std::path::PathBuf, &str, i64)> = Vec::new();
    let mut dirs = vec![vm.external_root.clone()];
    while let Some(d) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
                continue;
            }
            let ext = p
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_lowercase();
            let (mime, mtype) = match ext.as_str() {
                "jpg" | "jpeg" => ("image/jpeg", 1i64),
                "png" => ("image/png", 1),
                "gif" => ("image/gif", 1),
                "webp" => ("image/webp", 1),
                "bmp" => ("image/bmp", 1),
                "mp4" | "m4v" => ("video/mp4", 3),
                "mkv" => ("video/x-matroska", 3),
                "webm" => ("video/webm", 3),
                "3gp" => ("video/3gpp", 3),
                "mp3" => ("audio/mpeg", 2),
                "m4a" | "aac" => ("audio/mp4a-latm", 2),
                "ogg" | "opus" => ("audio/ogg", 2),
                "flac" => ("audio/flac", 2),
                "wav" => ("audio/x-wav", 2),
                _ => continue,
            };
            files.push((p, mime, mtype));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    // Aggregate counts for the MediaStore.Audio artists/albums/genres
    // content URIs: every audio file's artist is the conventional
    // unknown-artist label and its album is the parent folder, so the
    // artist row spans all audio files and each album row spans its
    // bucket.
    let audio_total = files.iter().filter(|f| f.2 == 2).count() as i64;
    // Keyed by the full relative parent dir (the bucket_id identity),
    // not the basename: "a/Music/" and "b/Music/" are different buckets.
    let mut bucket_audio: std::collections::BTreeMap<String, i64> =
        std::collections::BTreeMap::new();
    for f in files.iter().filter(|f| f.2 == 2) {
        let rel =
            f.0.strip_prefix(&vm.external_root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
        let dir = rel
            .rsplit_once('/')
            .map(|(d, _)| format!("{d}/"))
            .unwrap_or_default();
        *bucket_audio.entry(dir).or_default() += 1;
    }
    let audio_albums = bucket_audio.len() as i64;
    for (p, mime, mtype) in files {
        // A scoped volume (images/video/audio) filters by media
        // type; "file"/"media" volumes accept everything.
        let wanted = if images {
            mtype == 1
        } else if video {
            mtype == 3
        } else if audio {
            mtype == 2
        } else {
            true
        };
        if !wanted {
            continue;
        }
        let rel = p
            .strip_prefix(&vm.external_root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let title = p
            .file_stem()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let bucket = p
            .parent()
            .and_then(|x| x.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // Full parent path relative to the storage root, with a
        // trailing slash — nested folders keep their ancestors
        // (DCIM/Camera/ → "DCIM/Camera/", root files → "").
        let rel_dir = rel
            .rsplit_once('/')
            .map(|(d, _)| format!("{d}/"))
            .unwrap_or_default();
        let meta = std::fs::metadata(&p).ok();
        let size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
        let mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (w, h) = img_dims(&p);
        let dur = if mtype == 2 {
            audio_duration_ms(&p, size)
        } else {
            0
        };
        let data = format!("/storage/emulated/0/{rel}");
        // `_id` persists per path for the runtime's life — like a
        // real MediaStore rowid, adding/removing other files never
        // reshuffles it. `bucket_id` follows MediaStore's
        // parent-path hashCode (AOSP collides there too).
        let row_id = if let Some(&id) = vm.media_ids.get(&data) {
            id
        } else {
            vm.next_media_id += 1;
            vm.media_ids.insert(data.clone(), vm.next_media_id);
            vm.next_media_id
        };
        let bucket_id = java_hash(&rel_dir.to_lowercase()) as i64;
        // Scoped aggregate URIs key `_id` by their own entity:
        // `audio/albums` rows must expose `album_id` as `_id` —
        // apps join `track.album_id == album._id` (and likewise
        // artists/genres); a media rowid would fail that join.
        // `…/{id}/members` lists tracks — those rows keep their own
        // media rowid or playback URIs break.
        let album_id = java_hash(&rel_dir.to_lowercase()) as i64;
        let artist_id = 1i64;
        let genre_id = 0i64;
        let scoped_id = if uri.contains("/members") {
            row_id
        } else if uri.contains("/albums") {
            album_id
        } else if uri.contains("/artists") {
            artist_id
        } else if uri.contains("/genres") {
            genre_id
        } else {
            row_id
        };
        rows.push(vec![
            V::I64(scoped_id),
            jstr(vm, &data),
            jstr(vm, &name),
            jstr(vm, &title),
            V::I64(size),
            V::I64(mtime),
            V::I64(mtime),
            V::I64(mtime * 1000),
            jstr(vm, mime),
            V::I64(mtype),
            V::I64(bucket_id),
            jstr(vm, &bucket),
            jstr(vm, &rel_dir),
            V::I64(w),
            V::I64(h),
            V::I64(0),
            V::I64(dur),
            V::I64(0),
            jstr(vm, "external"),
            // The parent folder doubles as the album name (like a
            // Music/<Album>/<Track>.mp3 library layout); artist is
            // left at the conventional unknown-artist label.
            jstr(vm, "<unknown>"),
            jstr(vm, &bucket),
            V::I64(java_hash(&rel_dir.to_lowercase()) as i64),
            V::I64(1),
            // `track` = disc*1000 + number (MediaStore encoding);
            // apps filter `track > 0` — 0 drops the row entirely.
            V::I64(if mtype == 2 { 1001 } else { 0 }),
            V::I64(0),
            V::I64(0),
            jstr(vm, "<unknown>"),
            V::Ref(0),
            V::I64((mtype == 2) as i64),
            V::I64(0),
            V::I64(0),
            V::I64(0),
            V::I64(0),
            V::I64(0),
            V::I64(if mtype == 2 { audio_total } else { 0 }),
            V::I64(if mtype == 2 { audio_albums } else { 0 }),
            V::I64(if mtype == 2 {
                *bucket_audio.get(&rel_dir).unwrap_or(&0)
            } else {
                0
            }),
            V::I64(0),
            jstr(vm, "<unknown>"),
            V::I64(0),
        ]);
    }
    if crate::dbg_flag("OVELO_DRAINDBG") {
        eprintln!(
            "[media_cursor] {uri} rows={} projection={projection:?}",
            rows.len()
        );
    }
    // Honor the projection: the cursor's column set is exactly the
    // requested names, in the requested order — positional reads must
    // land on the right column. Unknown names yield null cells.
    let (cols, rows) = if projection.is_empty() || projection.iter().any(|c| c == "*") {
        (COLS.iter().map(|c| c.to_string()).collect(), rows)
    } else {
        let idxs: Vec<Option<usize>> = projection
            .iter()
            .map(|name| {
                COLS.iter()
                    .position(|c| c == name)
                    .or_else(|| COLS.iter().position(|c| c.eq_ignore_ascii_case(name)))
            })
            .collect();
        let proj_rows: Vec<Vec<V>> = rows
            .iter()
            .map(|r| {
                idxs.iter()
                    .map(|i| i.map(|i| r[i]).unwrap_or(V::Ref(0)))
                    .collect()
            })
            .collect();
        (projection.to_vec(), proj_rows)
    };
    let col_v: Vec<V> = cols.iter().map(|c| jstr(vm, c)).collect();
    let col_arr = arr(vm, "Ljava/lang/String;", col_v);
    let row_refs: Vec<V> = rows
        .into_iter()
        .map(|r| arr(vm, "Ljava/lang/Object;", r))
        .collect();
    let rows_l = host(vm, "Ljava/util/ArrayList;", HostState::List(row_refs));
    let V::Ref(c) = host(vm, "Landroid/database/Cursor;", HostState::Empty) else {
        unreachable!()
    };
    pseudo_set(vm, c, P_CUR_COLS, col_arr);
    pseudo_set(vm, c, P_CUR_ROWS, rows_l);
    pseudo_set(vm, c, P_CUR_POS, V::I32(-1));
    V::Ref(c)
}

/// Percent-decode (`%XX`) — byte-wise so multi-byte UTF-8 names
/// (`%E9%8C%B2%E9%9F%B3.wav` → `録音.wav`) survive the round-trip.
pub(crate) fn pct_dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a docId for a URI path segment — every byte outside
/// the RFC 3986 unreserved set is escaped so a docId's `/` never forges
/// a `/document/` or `/children` boundary and names containing `%` or
/// reserved words stay opaque.
pub(crate) fn pct_enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

/// Parse a `content://com.android.externalstorage.documents/…` URI into
/// `(doc_id, rel_path)`: the docId is the remainder after the last
/// `/document/` (or `/tree/` for a bare tree URI), percent-decoded —
/// docIds embed `/` (`primary:Music/Recordings`). `rel_path` drops
/// empty, `.` and `..` components so a guest-supplied id like
/// `primary:../../etc` cannot escape `external_root`.
pub(crate) fn saf_doc_rel(uri: &str) -> (String, String) {
    let doc_part = uri.strip_suffix("/children").unwrap_or(uri);
    let after = |pat: &str| -> Option<String> {
        doc_part
            .rfind(pat)
            .map(|off| pct_dec(&doc_part[off + pat.len()..]))
    };
    let doc_id = after("/document/")
        .or_else(|| after("/tree/"))
        .unwrap_or_default();
    let rel_raw = doc_id.split_once(':').map(|(_, p)| p).unwrap_or_default();
    let rel = rel_raw
        .split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .collect::<Vec<_>>()
        .join("/");
    (doc_id, rel)
}

/// `ContentResolver.query(content://com.android.externalstorage.documents/...)`
/// — SAF `DocumentsContract.Document` rows over `external_root`, used by
/// androidx `DocumentFile.fromTreeUri`/`findFile`/`listFiles` (fossify's
/// `getDocumentSdk30` walk).
///
/// URI shapes served (see the minting in `DocumentsContract` builtins):
///   `…/tree/{treeDocId}`                       — tree root document
///   `…/tree/{treeDocId}/document/{docId}`      — single document row
///   `…/tree/{treeDocId}/document/{docId}/children` — directory listing
///   `…/document/{docId}`                       — bare document row
/// DocIds are `{volume}:{relPath}` percent-encoded in the URI
/// (`primary%3AMusic%2FRecordings`), so a document literally named
/// `children` or containing `%`/`/document/` cannot collide with the
/// reserved path segments.
pub(crate) fn saf_cursor(vm: &mut Vm, uri: &str, projection: &[String]) -> V {
    const COLS: &[&str] = &[
        "document_id",
        "mime_type",
        "_display_name",
        "last_modified",
        "flags",
        "_size",
        "summary",
        "icon",
    ];
    // DocumentFile's `listFiles`/`findFile`/`exists`/`isDirectory` read
    // exactly these columns; unknown projection names yield null cells
    // like `media_cursor`.
    let children = uri.ends_with("/children");
    let (doc_id, rel) = saf_doc_rel(uri);
    let base = if rel.is_empty() {
        vm.external_root.clone()
    } else {
        vm.external_root.join(&rel)
    };
    let doc_mime = |p: &std::path::Path| -> &'static str {
        if p.is_dir() {
            return "vnd.android.document/directory";
        }
        match p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "mp4" | "m4v" => "video/mp4",
            "mkv" => "video/x-matroska",
            "webm" => "video/webm",
            "3gp" => "video/3gpp",
            "mp3" => "audio/mpeg",
            "m4a" | "aac" => "audio/mp4a-latm",
            "ogg" | "opus" => "audio/ogg",
            "flac" => "audio/flac",
            "wav" => "audio/x-wav",
            "txt" | "md" | "log" => "text/plain",
            "pdf" => "application/pdf",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        }
    };
    let vol = doc_id.split_once(':').map(|(v, _)| v).unwrap_or("primary");
    let push_row = |vm: &mut Vm, rows: &mut Vec<Vec<V>>, p: &std::path::Path, id: String| {
        let meta = std::fs::metadata(p).ok();
        let size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
        let mtime_ms = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "primary".to_string());
        let mime = doc_mime(p);
        // Document flags: dir → supports create/write/delete;
        // file → write/delete.
        let flags = if p.is_dir() { 14i64 } else { 6 };
        rows.push(vec![
            jstr(vm, &id),
            jstr(vm, mime),
            jstr(vm, &name),
            V::I64(mtime_ms),
            V::I64(flags),
            V::I64(if p.is_dir() { 0 } else { size }),
            V::Ref(0),
            V::I64(0),
        ]);
    };
    let mut rows: Vec<Vec<V>> = Vec::new();
    if children {
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let cid = if rel.is_empty() {
                    format!("{vol}:{name}")
                } else {
                    format!("{vol}:{rel}/{name}")
                };
                push_row(vm, &mut rows, &p, cid);
            }
        }
    } else if base.exists() {
        push_row(vm, &mut rows, &base, doc_id.clone());
    }
    if crate::dbg_flag("OVELO_DRAINDBG") {
        eprintln!(
            "[saf_cursor] {uri} rows={} projection={projection:?}",
            rows.len()
        );
    }
    let (cols, rows) = if projection.is_empty() || projection.iter().any(|c| c == "*") {
        (COLS.iter().map(|c| c.to_string()).collect(), rows)
    } else {
        let idxs: Vec<Option<usize>> = projection
            .iter()
            .map(|name| {
                COLS.iter()
                    .position(|c| c == name)
                    .or_else(|| COLS.iter().position(|c| c.eq_ignore_ascii_case(name)))
            })
            .collect();
        let proj_rows: Vec<Vec<V>> = rows
            .iter()
            .map(|r| {
                idxs.iter()
                    .map(|i| i.map(|i| r[i]).unwrap_or(V::Ref(0)))
                    .collect()
            })
            .collect();
        (projection.to_vec(), proj_rows)
    };
    let col_v: Vec<V> = cols.iter().map(|c| jstr(vm, c)).collect();
    let col_arr = arr(vm, "Ljava/lang/String;", col_v);
    let row_refs: Vec<V> = rows
        .into_iter()
        .map(|r| arr(vm, "Ljava/lang/Object;", r))
        .collect();
    let rows_l = host(vm, "Ljava/util/ArrayList;", HostState::List(row_refs));
    let V::Ref(c) = host(vm, "Landroid/database/Cursor;", HostState::Empty) else {
        unreachable!()
    };
    pseudo_set(vm, c, P_CUR_COLS, col_arr);
    pseudo_set(vm, c, P_CUR_ROWS, rows_l);
    pseudo_set(vm, c, P_CUR_POS, V::I32(-1));
    V::Ref(c)
}

/// MediaStore `duration` (ms) — zero makes music apps filter the row
/// out ("0 tracks"). WAV parses the real fmt/data chunks; compressed
/// formats estimate from size at ~128kbps so the value stays plausible.
fn audio_duration_ms(p: &std::path::Path, size: i64) -> i64 {
    use std::io::Read;
    let mut b = [0u8; 4096];
    if let Ok(mut f) = std::fs::File::open(p) {
        if let Ok(n) = f.read(&mut b) {
            let b = &b[..n];
            if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WAVE" {
                // Walk RIFF chunks — JUNK/LIST/etc. may sit between fmt
                // and data, so fixed offsets misread sizes.
                let (mut rate, mut data) = (0u64, 0u64);
                let mut i = 12usize;
                while i + 8 <= b.len() {
                    let tag = &b[i..i + 4];
                    let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
                    if tag == b"fmt " && i + 8 + len <= b.len() && len >= 16 {
                        rate =
                            u32::from_le_bytes(b[i + 8 + 8..i + 8 + 12].try_into().unwrap()) as u64;
                    } else if tag == b"data" {
                        data = len as u64;
                    }
                    i += 8 + len + (len & 1);
                }
                if rate > 0 && data > 0 {
                    return (data * 1000 / rate) as i64;
                }
            }
        }
    }
    if size > 0 {
        (size * 8 / 128).max(1)
    } else {
        0
    }
}

/// PNG/JPEG/GIF header dims without an image crate.
fn img_dims(p: &std::path::Path) -> (i64, i64) {
    let Ok(f) = std::fs::File::open(p) else {
        return (0, 0);
    };
    use std::io::Read;
    let mut buf = [0u8; 65536];
    let Ok(n) = f.take(65536).read(&mut buf) else {
        return (0, 0);
    };
    let b = &buf[..n];
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.len() >= 24 {
        let w = u32::from_be_bytes(b[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(b[20..24].try_into().unwrap());
        return (w as i64, h as i64);
    }
    if b.starts_with(b"GIF8") && b.len() >= 10 {
        let w = u16::from_le_bytes(b[6..8].try_into().unwrap());
        let h = u16::from_le_bytes(b[8..10].try_into().unwrap());
        return (w as i64, h as i64);
    }
    if b.starts_with(b"BM") && b.len() >= 26 {
        let w = i32::from_le_bytes(b[18..22].try_into().unwrap());
        let h = i32::from_le_bytes(b[22..26].try_into().unwrap()).abs();
        return (w as i64, h as i64);
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        // JPEG: walk SOI→segment markers until a SOF carries dims.
        let mut i = 2usize;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC
            {
                let h = u16::from_be_bytes(b[i + 5..i + 7].try_into().unwrap());
                let w = u16::from_be_bytes(b[i + 7..i + 9].try_into().unwrap());
                return (w as i64, h as i64);
            }
            if i + 4 > b.len() {
                break;
            }
            let len = u16::from_be_bytes(b[i + 2..i + 4].try_into().unwrap()) as usize;
            if len < 2 {
                break;
            }
            i += 2 + len;
        }
    }
    (0, 0)
}

/// Build `SELECT …` for `query*` overloads: scan args for
/// (table, columns[], selection, selectionArgs[], groupBy, having,
/// orderBy, limit) in order.
fn build_query(vm: &Vm, args: &[V]) -> Option<(String, V)> {
    let mut strings = Vec::new();
    let mut arrays = Vec::new();
    for a in &args[1..] {
        match a {
            V::Ref(0) => {}
            V::Ref(h) if matches!(vm.heap.get(*h), Some(Obj::Array { .. })) => arrays.push(*a),
            V::Ref(h) if s(vm, *h).is_some() => strings.push(s(vm, *h).unwrap_or_default()),
            _ => {}
        }
    }
    let table = strings.first()?.clone();
    let col_list = arrays
        .first()
        .and_then(|a| match a {
            V::Ref(h) => match vm.heap.get(*h) {
                Some(Obj::Array { data, .. }) => Some(
                    data.iter()
                        .filter_map(|v| sv(vm, *v))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                _ => None,
            },
            _ => None,
        })
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "*".to_string());
    let mut sql = format!("SELECT {col_list} FROM \"{table}\"");
    // strings after table: selection, groupBy, having, orderBy, limit.
    let rest = &strings[1..];
    if let Some(sel) = rest.first() {
        if !sel.is_empty() {
            sql.push_str(&format!(" WHERE {sel}"));
        }
    }
    let mut tail = rest.iter().skip(1);
    if let Some(g) = tail.next() {
        if !g.is_empty() {
            sql.push_str(&format!(" GROUP BY {g}"));
        }
        if let Some(h) = tail.next() {
            if !h.is_empty() {
                sql.push_str(&format!(" HAVING {h}"));
            }
            if let Some(o) = tail.next() {
                if !o.is_empty() {
                    sql.push_str(&format!(" ORDER BY {o}"));
                }
                if let Some(l) = tail.next() {
                    if !l.is_empty() {
                        sql.push_str(&format!(" LIMIT {l}"));
                    }
                }
            }
        }
    }
    let bind_v = arrays.get(1).copied().unwrap_or(V::Ref(0));
    Some((sql, bind_v))
}

/// ContentValues-like object → ordered (column, value) pairs.
fn content_values(vm: &Vm, v: V) -> Vec<(String, V)> {
    let V::Ref(h) = v else { return Vec::new() };
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => m
            .iter()
            .filter_map(|(k, v)| sv(vm, *k).map(|k| (k, *v)))
            .collect(),
        _ => Vec::new(),
    }
}

fn find_map_arg(vm: &Vm, args: &[V]) -> V {
    args.iter()
        .find(|a| {
            matches!(a, V::Ref(h) if matches!(
                vm.heap.get(*h),
                Some(Obj::Host { state: HostState::Map(_), .. })
            ))
        })
        .copied()
        .unwrap_or(V::Ref(0))
}

fn find_array_arg(vm: &Vm, args: &[V]) -> V {
    args.iter()
        .rev()
        .find(|a| matches!(a, V::Ref(h) if matches!(vm.heap.get(*h), Some(Obj::Array{..}))))
        .copied()
        .unwrap_or(V::Ref(0))
}

/// Single entry: `android.database.*` + `androidx.sqlite` receivers.
/// Routes by receiver shape — db-bound objects get sqlite ops,
/// P_SQL-carrying ones get statement ops, everything else cursor ops.
pub(crate) fn db_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let h = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    if crate::dbg_flag("OVELO_DBDBG") {
        let sql = args.get(1).and_then(|v| sv(vm, *v)).unwrap_or_default();
        let sql: String = sql.chars().take(80).collect();
        eprintln!("[db] {name}(h={h}) {sql}");
    }
    // A SQLiteStatement also lands here (database/* dispatch): it
    // carries P_SQL, so it takes the statement path.
    if pseudo_get(vm, h, P_SQL).is_some() {
        return stmt_call(vm, name, args);
    }
    if vm.obj_db.contains_key(&h) || name == "<init>" {
        let r = sqlite_call(vm, name, args)?;
        if r.is_some() {
            return Ok(r);
        }
    }
    if let Some(r) = sqlite_call(vm, name, args)? {
        return Ok(Some(r));
    }
    cursor_call(vm, name, args)
}

/// `SQLiteOpenHelper` / `SQLiteDatabase` methods.
fn sqlite_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    Ok(match name {
        "<init>" => {
            // SQLiteOpenHelper(context, name, factory, version): record
            // the db name + schema version so getWritableDatabase can
            // find the connection and run onCreate/onUpgrade.
            if let Some(n) = args.get(2).and_then(|v| sv(vm, *v)) {
                bind_name(vm, this, &n);
            }
            if let Some(&v) = args.get(4) {
                pseudo_set(vm, this, P_DBVER, V::I32(i32v(v)));
            }
            Some(None)
        }
        "getWritableDatabase" | "getReadableDatabase" => {
            let n = db_name_of(vm, this).unwrap_or_else(|| "ovelo".into());
            let out = db_obj(vm, &n)?;
            // AOSP migrates on open: when the stored user_version
            // differs from the helper's schema version the matching
            // guest callback runs before the db is handed out.
            let want = pseudo_get(vm, this, P_DBVER).map(i32v).unwrap_or(0);
            if want > 0 {
                let cur: i32 = conn_of(vm, this)
                    .ok()
                    .map(|d| {
                        d.borrow()
                            .conn
                            .query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0))
                            .unwrap_or(0) as i32
                    })
                    .unwrap_or(0);
                if cur != want {
                    let (hook, cb_args): (&str, Vec<V>) = if cur == 0 {
                        ("onCreate", vec![out])
                    } else if cur < want {
                        ("onUpgrade", vec![out, V::I32(cur), V::I32(want)])
                    } else {
                        ("onDowngrade", vec![out, V::I32(cur), V::I32(want)])
                    };
                    // The whole migration runs in one transaction — a
                    // failed callback rolls back its partial schema.
                    if let Ok(db) = conn_of(vm, this) {
                        let mut d = db.borrow_mut();
                        if d.tx_stack.is_empty() {
                            let _ = d.conn.execute_batch("BEGIN");
                        }
                        d.tx_stack.push(true);
                    }
                    // Resolve the hook up front: a void callback that
                    // ran successfully also yields Ok(None), so only a
                    // missing method may be treated as "no migration".
                    let ci = vm.heap.class_of(this).unwrap_or(0);
                    let has_hook = vm
                        .resolve_call_args(ci, hook, &cb_args)
                        .ok()
                        .and_then(|(m, _)| m)
                        .is_some();
                    let res = if has_hook {
                        vm.call_virtual_rt(this, hook, &cb_args)
                    } else {
                        Ok(None)
                    };
                    let ok = res.is_ok();
                    if let Ok(db) = conn_of(vm, this) {
                        let mut d = db.borrow_mut();
                        let _ = d.tx_stack.pop();
                        if let Some(top) = d.tx_stack.last_mut() {
                            *top &= ok;
                        } else if let Err(e) = commit_or_rollback(&mut d.conn, ok) {
                            // A failed migration COMMIT must surface —
                            // stamping user_version over a rolled-back
                            // schema would skip the migration forever.
                            return Err(sql_err(vm, e));
                        }
                    }
                    match res {
                        Ok(_) if has_hook => {
                            if let Ok(db) = conn_of(vm, this) {
                                let _ = db
                                    .borrow()
                                    .conn
                                    .execute_batch(&format!("PRAGMA user_version={want}"));
                            }
                        }
                        // A helper without the callback can't perform
                        // the migration — fail like AOSP instead of
                        // stamping a version the schema doesn't match.
                        Ok(_) => {
                            return Err(sql_err(
                                vm,
                                format!(
                                    "helper provides no {hook}; cannot migrate {n} {cur}->{want}"
                                ),
                            ));
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Some(Some(out))
        }
        "beginTransaction"
        | "beginTransactionNonExclusive"
        | "beginTransactionWithListener"
        | "beginTransactionWithListenerNonExclusive" => {
            let db = conn_of(vm, this)?;
            let mut d = db.borrow_mut();
            // Nested begins stack on one level each — only the
            // outermost pair issues BEGIN/COMMIT/ROLLBACK.
            if d.tx_stack.is_empty() {
                d.conn.execute_batch("BEGIN").map_err(|e| sql_err(vm, e))?;
            }
            d.tx_stack.push(false);
            Some(None)
        }
        "setTransactionSuccessful" => {
            if let Ok(db) = conn_of(vm, this) {
                if let Some(top) = db.borrow_mut().tx_stack.last_mut() {
                    *top = true;
                }
            }
            Some(None)
        }
        "endTransaction" => {
            let db = conn_of(vm, this)?;
            let mut d = db.borrow_mut();
            let Some(ok) = d.tx_stack.pop() else {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IllegalStateException;",
                    "endTransaction without beginTransaction",
                ));
            };
            if let Some(top) = d.tx_stack.last_mut() {
                // Fold this level into the parent — an unmarked inner
                // transaction poisons the outer one's outcome, and a
                // later setTransactionSuccessful can't undo that.
                *top &= ok;
                if !ok {
                    d.tx_failed = true;
                }
            } else {
                let commit = ok && !d.tx_failed;
                commit_or_rollback(&mut d.conn, commit).map_err(|e| sql_err(vm, e))?;
                d.tx_failed = false;
            }
            Some(None)
        }
        "inTransaction" => {
            let t = conn_of(vm, this)
                .map(|d| !d.borrow().tx_stack.is_empty())
                .unwrap_or(false);
            Some(Some(V::I32(t as i32)))
        }
        "getDatabaseName" => Some(Some(match db_name_of(vm, this) {
            Some(n) => jstr(vm, &n),
            None => V::Ref(0),
        })),
        "openOrCreateDatabase" | "openDatabase" => {
            let n = args
                .iter()
                .skip(1)
                .find_map(|a| sv(vm, *a))
                .unwrap_or_default();
            Some(Some(open_or_create(vm, &n)?))
        }
        "execSQL" => {
            let sql = first_string(vm, &args[1..]).unwrap_or_default();
            let db = conn_of(vm, this)?;
            if let Some(&binds) = args.get(2).filter(|b| !is_null(**b)) {
                let params = binds_of(vm, binds);
                db.borrow()
                    .conn
                    .execute(&sql, params_from_iter(params))
                    .map_err(|e| sql_err(vm, e))?;
            } else {
                db.borrow()
                    .conn
                    .execute_batch(&sql)
                    .map_err(|e| sql_err(vm, e))?;
            }
            Some(None)
        }
        "rawQuery" | "rawQueryWithFactory" => {
            let sql = first_string(vm, &args[1..]).unwrap_or_default();
            let mut binds = find_array_arg(vm, &args[2..]);
            // androidx `query(SupportSQLiteQuery)` reaches us as
            // `rawQueryWithFactory(factory, sql, EMPTY, …)`: the binds
            // aren't in args — the guest factory's `bindTo` attaches
            // them to the SQLiteQuery program `newInstance` receives.
            // Invoke the factory on a host program to capture them.
            if binds_of(vm, binds).is_empty() && name.ends_with("Factory") {
                if let Some(&V::Ref(factory)) = args.get(1) {
                    let V::Ref(prog) = host(
                        vm,
                        "Landroid/database/sqlite/SQLiteQuery;",
                        HostState::List(Vec::new()),
                    ) else {
                        unreachable!()
                    };
                    let sql_v = jstr(vm, &sql);
                    pseudo_set(vm, prog, P_SQL, sql_v);
                    if let Some(n) = db_name_of(vm, this) {
                        vm.obj_db.insert(prog, n);
                    }
                    // androidx's generated factory is a `CursorFactory`
                    // — `newCursor` binds the query onto the program.
                    // `newInstance` covers the alternate driver shape.
                    // call_virtual answers Ok(None) when the name is
                    // unresolved, so gate the fallback on the binds
                    // actually landing in the program's list.
                    let _ = vm.call_virtual(
                        factory,
                        "newCursor",
                        &[V::Ref(this), V::Ref(0), V::Ref(0), V::Ref(prog)],
                    );
                    let bound = matches!(
                        vm.heap.get(prog),
                        Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) if !l.is_empty()
                    );
                    if !bound {
                        let _ = vm.call_virtual(
                            factory,
                            "newInstance",
                            &[V::Ref(this), V::Ref(0), V::Ref(0), V::Ref(prog)],
                        );
                    }
                    if matches!(
                        vm.heap.get(prog),
                        Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) if !l.is_empty()
                    ) {
                        binds = V::Ref(prog);
                    }
                }
            }
            Some(Some(run_query(vm, this, &sql, binds)?))
        }
        "query" | "queryWithFactory" => {
            let Some((sql, binds)) = build_query(vm, args) else {
                return Ok(None);
            };
            Some(Some(run_query(vm, this, &sql, binds)?))
        }
        "compileStatement" => {
            let sql = first_string(vm, &args[1..]).unwrap_or_default();
            let V::Ref(st) = host(
                vm,
                "Landroid/database/sqlite/SQLiteStatement;",
                HostState::List(Vec::new()),
            ) else {
                unreachable!()
            };
            let sql_v = jstr(vm, &sql);
            pseudo_set(vm, st, P_SQL, sql_v);
            if let Some(n) = db_name_of(vm, this) {
                vm.obj_db.insert(st, n);
            }
            Some(Some(V::Ref(st)))
        }
        "insert" | "insertOrThrow" | "insertWithOnConflict" | "replace" | "replaceOrThrow" => {
            let t = first_string(vm, &args[1..]).unwrap_or_default();
            let pairs = content_values(vm, find_map_arg(vm, args));
            let verb = if name.starts_with("replace") {
                "REPLACE"
            } else {
                "INSERT"
            };
            let (sql, params): (String, Vec<SV>) = if pairs.is_empty() {
                (format!("{verb} INTO \"{t}\" DEFAULT VALUES"), Vec::new())
            } else {
                let cols = pairs
                    .iter()
                    .map(|(k, _)| format!("\"{k}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let qs = pairs.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                (
                    format!("{verb} INTO \"{t}\" ({cols}) VALUES ({qs})"),
                    pairs.iter().map(|(_, v)| v_to_sql(vm, *v)).collect(),
                )
            };
            let db = conn_of(vm, this)?;
            db.borrow()
                .conn
                .execute(&sql, params_from_iter(params))
                .map_err(|e| sql_err(vm, e))?;
            let id = db.borrow().conn.last_insert_rowid();
            Some(Some(V::I64(id)))
        }
        "update" | "updateWithOnConflict" => {
            let t = first_string(vm, &args[1..]).unwrap_or_default();
            let pairs = content_values(vm, find_map_arg(vm, args));
            let where_ = args
                .iter()
                .filter_map(|a| sv(vm, *a))
                .nth(1)
                .unwrap_or_default();
            let set = pairs
                .iter()
                .map(|(k, _)| format!("\"{k}\"=?"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut sql = format!("UPDATE \"{t}\" SET {set}");
            if !where_.is_empty() {
                sql.push_str(&format!(" WHERE {where_}"));
            }
            let mut params: Vec<SV> = pairs.iter().map(|(_, v)| v_to_sql(vm, *v)).collect();
            params.extend(binds_of(vm, find_array_arg(vm, args)));
            let db = conn_of(vm, this)?;
            let n = db
                .borrow()
                .conn
                .execute(&sql, params_from_iter(params))
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(V::I32(n as i32)))
        }
        "delete" => {
            let t = first_string(vm, &args[1..]).unwrap_or_default();
            let where_ = args
                .iter()
                .filter_map(|a| sv(vm, *a))
                .nth(1)
                .unwrap_or_default();
            let mut sql = format!("DELETE FROM \"{t}\"");
            if !where_.is_empty() {
                sql.push_str(&format!(" WHERE {where_}"));
            }
            let db = conn_of(vm, this)?;
            let n = db
                .borrow()
                .conn
                .execute(
                    &sql,
                    params_from_iter(binds_of(vm, find_array_arg(vm, args))),
                )
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(V::I32(n as i32)))
        }
        "isOpen" => Some(Some(V::I32(1))),
        "close" => {
            if let Some(n) = db_name_of(vm, this) {
                vm.dbs.remove(&safe_name(&n));
            }
            Some(None)
        }
        "getVersion" => {
            let db = conn_of(vm, this)?;
            let v: i64 = db
                .borrow()
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap_or(0);
            Some(Some(V::I32(v as i32)))
        }
        "setVersion" => {
            let db = conn_of(vm, this)?;
            let _ = db.borrow().conn.execute_batch(&format!(
                "PRAGMA user_version={}",
                i32v(*args.get(1).unwrap_or(&V::I32(0)))
            ));
            Some(None)
        }
        "getPath" => {
            let n = db_name_of(vm, this).unwrap_or_default();
            Some(Some(jstr(vm, &format!("/data/local/ovelo/databases/{n}"))))
        }
        "getMaximumSize" | "getPageSize" => Some(Some(V::I64(0))),
        "isReadOnly"
        | "isDbLockedByCurrentThread"
        | "isDbLockedByOtherThreads"
        | "isWriteAheadLoggingEnabled" => Some(Some(V::I32(0))),
        "isDatabaseIntegrityOk" | "yieldIfContendedSafely" => Some(Some(V::I32(1))),
        "setForeignKeyConstraintsEnabled"
        | "enableWriteAheadLogging"
        | "disableWriteAheadLogging"
        | "setMaximumSize"
        | "setPageSize"
        | "setIdleConnectionTimeout"
        | "setLocale"
        | "setLockingEnabled"
        | "acquireReference"
        | "releaseReference"
        | "releaseMemory"
        | "setWriteAheadLoggingEnabled"
        | "markTableSyncable" => Some(None),
        "getAttachedDbs" | "getSyncedTables" => Some(Some(V::Ref(0))),
        "needUpgrade" => Some(Some(V::I32(0))),
        "findEditTable" => Some(Some(
            first_string(vm, args)
                .map(|s| jstr(vm, &s))
                .unwrap_or(V::Ref(0)),
        )),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// SQLiteStatement / SQLiteProgram (bind + execute)

fn stmt_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    if name.starts_with("bind") || name == "clearBindings" {
        // Binds live in the statement's HostState::List.
        if let Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) = vm.heap.get_mut(h)
        {
            if name == "clearBindings" {
                l.clear();
            } else {
                let idx = i32v(*args.get(1).unwrap_or(&V::I32(0))) as usize;
                let val = args.get(2).copied().unwrap_or(V::Ref(0));
                if idx >= 1 {
                    while l.len() < idx {
                        l.push(V::Ref(0));
                    }
                    l[idx - 1] = val;
                }
            }
        }
        return Ok(Some(None));
    }
    let sql = pseudo_get(vm, h, P_SQL)
        .and_then(|v| sv(vm, v))
        .unwrap_or_default();
    if sql.is_empty() {
        return Ok(None);
    }
    let params: Vec<SV> = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) => l.iter().map(|v| v_to_sql(vm, *v)).collect(),
        _ => Vec::new(),
    };
    let db = conn_of(vm, h)?;
    Ok(match name {
        "execute" => {
            db.borrow()
                .conn
                .execute(&sql, params_from_iter(params))
                .map_err(|e| sql_err(vm, e))?;
            Some(None)
        }
        "executeInsert" => {
            db.borrow()
                .conn
                .execute(&sql, params_from_iter(params))
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(V::I64(db.borrow().conn.last_insert_rowid())))
        }
        "executeUpdateDelete" => {
            let n = db
                .borrow()
                .conn
                .execute(&sql, params_from_iter(params))
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(V::I32(n as i32)))
        }
        "simpleQueryForLong" | "simpleQueryForInt" => {
            let d = db.borrow();
            let v: i64 = d
                .conn
                .query_row(&sql, params_from_iter(params), |r| r.get(0))
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(V::I64(v)))
        }
        "simpleQueryForString" => {
            let d = db.borrow();
            let s: String = d
                .conn
                .query_row(&sql, params_from_iter(params), |r| r.get::<_, String>(0))
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(jstr(vm, &s)))
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Cursor

fn cur_rows(vm: &Vm, h: u32) -> Vec<V> {
    match pseudo_get(vm, h, P_CUR_ROWS) {
        Some(V::Ref(rh)) => match vm.heap.get(rh) {
            Some(Obj::Host {
                state: HostState::List(l),
                ..
            }) => l.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn cur_cols(vm: &Vm, h: u32) -> Vec<String> {
    match pseudo_get(vm, h, P_CUR_COLS) {
        Some(V::Ref(ch)) => match vm.heap.get(ch) {
            Some(Obj::Array { data, .. }) => data.iter().filter_map(|v| sv(vm, *v)).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn cur_pos(vm: &Vm, h: u32) -> i32 {
    pseudo_get(vm, h, P_CUR_POS).map(i32v).unwrap_or(-1)
}

fn set_pos(vm: &mut Vm, h: u32, p: i32) {
    pseudo_set(vm, h, P_CUR_POS, V::I32(p));
}

fn cur_cell(vm: &Vm, h: u32, col: i32) -> V {
    let rows = cur_rows(vm, h);
    let pos = cur_pos(vm, h);
    if pos < 0 || pos as usize >= rows.len() || col < 0 {
        return V::Ref(0);
    }
    if let V::Ref(rh) = rows[pos as usize] {
        if let Some(Obj::Array { data, .. }) = vm.heap.get(rh) {
            return data.get(col as usize).copied().unwrap_or(V::Ref(0));
        }
    }
    V::Ref(0)
}

/// `android.database.Cursor` — every accessor reads the pseudo fields
/// populated by `run_query`.
fn cursor_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let n_rows = cur_rows(vm, h).len() as i32;
    let col_arg = i32v(*args.get(1).unwrap_or(&V::I32(0)));
    let dbg = crate::dbg_flag("OVELO_CURDBG");
    if dbg && matches!(name, "getCount") {
        eprintln!(
            "[cur] h={h} getCount -> {n_rows} cols={:?}",
            cur_cols(vm, h)
        );
    }
    Ok(match name {
        "getCount" => Some(Some(V::I32(n_rows))),
        "getPosition" => Some(Some(V::I32(cur_pos(vm, h)))),
        "moveToFirst" => {
            let ok = n_rows > 0;
            if ok {
                set_pos(vm, h, 0);
            }
            Some(Some(V::I32(ok as i32)))
        }
        "moveToLast" => {
            let ok = n_rows > 0;
            if ok {
                set_pos(vm, h, n_rows - 1);
            }
            Some(Some(V::I32(ok as i32)))
        }
        "moveToNext" => {
            let p = cur_pos(vm, h) + 1;
            if p < n_rows {
                set_pos(vm, h, p);
                Some(Some(V::I32(1)))
            } else {
                set_pos(vm, h, n_rows);
                Some(Some(V::I32(0)))
            }
        }
        "moveToPrevious" => {
            let p = cur_pos(vm, h) - 1;
            if p >= 0 {
                set_pos(vm, h, p);
                Some(Some(V::I32(1)))
            } else {
                set_pos(vm, h, -1);
                Some(Some(V::I32(0)))
            }
        }
        "moveToPosition" => {
            let p = col_arg;
            set_pos(vm, h, p.max(-1).min(n_rows));
            Some(Some(V::I32((p >= 0 && p < n_rows) as i32)))
        }
        "move" => {
            let p = cur_pos(vm, h) + col_arg;
            let ok = p >= 0 && p < n_rows;
            set_pos(vm, h, if ok { p } else { cur_pos(vm, h) });
            Some(Some(V::I32(ok as i32)))
        }
        "isFirst" => Some(Some(V::I32((cur_pos(vm, h) == 0 && n_rows > 0) as i32))),
        "isLast" => Some(Some(V::I32(
            (cur_pos(vm, h) == n_rows - 1 && n_rows > 0) as i32,
        ))),
        // AOSP AbstractCursor: an empty cursor is both before-first and
        // after-last. Returning false there makes guest loops like
        // `while (!isAfterLast()) { moveToNext() }` spin forever and
        // blow the interpreter's instruction budget.
        "isBeforeFirst" => Some(Some(V::I32((n_rows == 0 || cur_pos(vm, h) < 0) as i32))),
        "isAfterLast" => Some(Some(V::I32(
            (n_rows == 0 || cur_pos(vm, h) >= n_rows) as i32,
        ))),
        "getColumnIndex" | "getColumnIndexOrThrow" => {
            let want = sv(vm, *args.get(1).unwrap_or(&V::Ref(0))).unwrap_or_default();
            let cols = cur_cols(vm, h);
            let idx = cols
                .iter()
                .position(|c| c == &want)
                .or_else(|| cols.iter().position(|c| c.eq_ignore_ascii_case(&want)))
                .map(|i| i as i32)
                .unwrap_or(-1);
            if idx < 0 && name == "getColumnIndexOrThrow" {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IllegalArgumentException;",
                    &format!("column '{want}' does not exist"),
                ));
            }
            Some(Some(V::I32(idx)))
        }
        "getColumnName" => {
            let nm = cur_cols(vm, h)
                .get(col_arg as usize)
                .cloned()
                .unwrap_or_default();
            Some(Some(jstr(vm, &nm)))
        }
        "getColumnNames" => {
            let vs: Vec<V> = cur_cols(vm, h).iter().map(|c| jstr(vm, c)).collect();
            Some(Some(arr(vm, STRING, vs)))
        }
        "getColumnCount" => Some(Some(V::I32(cur_cols(vm, h).len() as i32))),
        "getString" => {
            let v = cur_cell(vm, h, col_arg);
            if dbg {
                eprintln!(
                    "[cur] h={h} getString pos={} col={col_arg} -> {}",
                    cur_pos(vm, h),
                    sv(vm, v)
                        .unwrap_or_else(|| format!("{v:?}"))
                        .chars()
                        .take(60)
                        .collect::<String>()
                );
            }
            Some(Some(match v {
                V::Ref(_) => v,
                V::I32(i) => jstr(vm, &i.to_string()),
                V::I64(i) => jstr(vm, &i.to_string()),
                V::F32(f) => jstr(vm, &f.to_string()),
                V::F64(f) => jstr(vm, &f.to_string()),
            }))
        }
        "getInt" | "getShort" => Some(Some(V::I32(i64v(cur_cell(vm, h, col_arg)) as i32))),
        "getLong" => Some(Some(V::I64(i64v(cur_cell(vm, h, col_arg))))),
        "getFloat" => Some(Some(V::F32(f64v(cur_cell(vm, h, col_arg)) as f32))),
        "getDouble" => Some(Some(V::F64(f64v(cur_cell(vm, h, col_arg))))),
        "getBlob" => Some(Some(cur_cell(vm, h, col_arg))),
        "isNull" => Some(Some(V::I32(
            matches!(cur_cell(vm, h, col_arg), V::Ref(0)) as i32
        ))),
        "getType" => Some(Some(V::I32(match cur_cell(vm, h, col_arg) {
            V::Ref(0) => 0,
            V::I32(_) | V::I64(_) => 1,
            V::F32(_) | V::F64(_) => 2,
            V::Ref(rh) if s(vm, rh).is_some() => 3,
            _ => 4,
        }))),
        "getExtras" | "getNotificationUri" | "respond" => Some(Some(V::Ref(0))),
        "requery" => Some(Some(V::I32(0))),
        "getWantsAllOnMoveCalls" => Some(Some(V::I32(0))),
        "copyStringToBuffer"
        | "close"
        | "deactivate"
        | "registerContentObserver"
        | "unregisterContentObserver"
        | "registerDataSetObserver"
        | "unregisterDataSetObserver"
        | "setNotificationUri"
        | "setExtras" => Some(None),
        _ => None,
    })
}

fn sv_ref(v: ValueRef) -> SV {
    match v {
        ValueRef::Null => SV::Null,
        ValueRef::Integer(i) => SV::Integer(i),
        ValueRef::Real(f) => SV::Real(f),
        ValueRef::Text(t) => SV::Text(String::from_utf8_lossy(t).to_string()),
        ValueRef::Blob(b) => SV::Blob(b.to_vec()),
    }
}

/// androidx.sqlite `BundledSQLite*` driver — JNI natives backed by the
/// same rusqlite connection pool as `SQLiteDatabase`. Room3's
/// `ConnectionPool` talks straight to these (the app's `libsqliteJni.so`
/// is never touched), so without them every Room query no-ops.
///
/// A statement handle carries its sql + pending binds; the whole result
/// is materialized on the first `step` (`run_query` semantics), later
/// steps walk the rows.
pub(crate) struct BundledStmt {
    pub conn: Rc<RefCell<Db>>,
    pub sql: String,
    pub params: Vec<SV>,
    pub cols: Vec<String>,
    pub rows: Vec<Vec<SV>>,
    /// Row a successful `step` landed on.
    pub cur: Option<usize>,
    /// The statement ran at least once since prepare/reset.
    pub executed: bool,
}

impl BundledStmt {
    fn exec(&mut self) -> rusqlite::Result<()> {
        let d = self.conn.borrow();
        if crate::dbg_flag("OVELO_DBDBG") {
            let sql: String = self.sql.chars().take(400).collect();
            eprintln!("[bundled] exec: {sql} | params={:?}", self.params);
        }
        let mut st = d.conn.prepare(&self.sql)?;
        if st.column_count() > 0 {
            self.cols = st.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = st.query(params_from_iter(self.params.clone()))?;
            self.rows.clear();
            while let Some(r) = rows.next()? {
                let mut row = Vec::with_capacity(self.cols.len());
                for i in 0..self.cols.len() {
                    row.push(sv_ref(r.get_ref(i)?));
                }
                self.rows.push(row);
            }
            if crate::dbg_flag("OVELO_DBDBG") {
                eprintln!(
                    "[bundled] query -> {} rows: {:?}",
                    self.rows.len(),
                    self.rows.first()
                );
            }
        } else {
            let r = st.execute(params_from_iter(self.params.clone()))?;
            if crate::dbg_flag("OVELO_DBDBG") {
                eprintln!(
                    "[bundled] exec -> {r} rows rowid={}",
                    d.conn.last_insert_rowid()
                );
            }
            self.cols = Vec::new();
            self.rows = Vec::new();
        }
        self.cur = None;
        self.executed = true;
        Ok(())
    }

    fn cur_row(&self) -> Option<&[SV]> {
        self.cur
            .and_then(|i| self.rows.get(i))
            .map(|r| r.as_slice())
    }

    fn cell(&self, col: i64) -> SV {
        self.cur_row()
            .and_then(|r| r.get(col.max(0) as usize))
            .cloned()
            .unwrap_or(SV::Null)
    }
}

fn sv_long(v: &SV) -> i64 {
    match v {
        SV::Integer(i) => *i,
        SV::Real(f) => *f as i64,
        SV::Text(t) => t.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn sv_double(v: &SV) -> f64 {
    match v {
        SV::Integer(i) => *i as f64,
        SV::Real(f) => *f,
        SV::Text(t) => t.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// SQLITE_COLUMN_* type of a cell (5 = NULL before/without a row).
fn sv_coltype(v: &SV) -> i32 {
    match v {
        SV::Integer(_) => 1,
        SV::Real(_) => 2,
        SV::Text(_) => 3,
        SV::Blob(_) => 4,
        SV::Null => 5,
    }
}

fn bconn(vm: &mut Vm, h: i64) -> Result<Rc<RefCell<Db>>, E> {
    vm.bundled_conns
        .get(&(h as u64))
        .cloned()
        .ok_or_else(|| sql_err(vm, format!("connection {h} is closed")))
}

fn next_bundled_h(vm: &mut Vm) -> i64 {
    vm.bundled_h += 1;
    vm.bundled_h as i64
}

/// `Landroidx/sqlite/driver/bundled/*Kt;` native declarations.
pub(crate) fn bundled_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let r: Result<Option<V>, E> = (|| {
        Ok(match name {
            "nativeThreadSafeMode" => Some(V::I32(1)),
            "nativeOpen" => {
                let p = args.first().and_then(|v| sv(vm, *v)).unwrap_or_default();
                let dbname = if p.contains(":memory:") {
                    ":memory:".to_string()
                } else {
                    let path = p.strip_prefix("file:").unwrap_or(&p);
                    safe_name(path.split('?').next().unwrap_or(path))
                };
                // NOT conn_for: every bundled open gets its own
                // Connection — Room's reader pool sets
                // `PRAGMA query_only = 1`, which would turn the shared
                // handle readonly for the writer too.
                let d = open_raw(vm, &dbname)?;
                let h = next_bundled_h(vm);
                vm.bundled_conns.insert(h as u64, d);
                Some(V::I64(h))
            }
            "nativeClose" => {
                let h = i64v(args[0]);
                if vm.bundled_stmts.remove(&(h as u64)).is_none() {
                    vm.bundled_conns.remove(&(h as u64));
                }
                None
            }
            "nativeInTransaction" => {
                let d = bconn(vm, i64v(args[0]))?;
                let auto = d.borrow().conn.is_autocommit();
                Some(V::I32(!auto as i32))
            }
            "nativeLoadExtension" => None,
            "nativePrepare" => {
                let d = bconn(vm, i64v(args[0]))?;
                let sql = args.get(1).and_then(|v| sv(vm, *v)).unwrap_or_default();
                if crate::dbg_flag("OVELO_DBDBG") {
                    let t: String = sql.chars().take(100).collect();
                    eprintln!("[bundled] prepare: {t}");
                }
                // Compile now: the real JNI surfaces syntax errors here,
                // not at first step.
                d.borrow().conn.prepare(&sql).map_err(|e| sql_err(vm, e))?;
                let h = next_bundled_h(vm);
                vm.bundled_stmts.insert(
                    h as u64,
                    BundledStmt {
                        conn: d,
                        sql,
                        params: Vec::new(),
                        cols: Vec::new(),
                        rows: Vec::new(),
                        cur: None,
                        executed: false,
                    },
                );
                Some(V::I64(h))
            }
            "nativeStep" => {
                let h = i64v(args[0]);
                if !vm.bundled_stmts.contains_key(&(h as u64)) {
                    return Err(sql_err(vm, format!("statement {h} is closed")));
                }
                let mut err = None;
                let has = {
                    let s = vm.bundled_stmts.get_mut(&(h as u64)).unwrap();
                    if !s.executed {
                        err = s.exec().err();
                        if crate::dbg_flag("OVELO_DBDBG") {
                            if let Some(e) = &err {
                                eprintln!("[bundled] exec ERR: {e}");
                            }
                        }
                    }
                    if err.is_none() {
                        let next = s.cur.map(|i| i + 1).unwrap_or(0);
                        let has = next < s.rows.len();
                        s.cur = if has { Some(next) } else { None };
                        has
                    } else {
                        false
                    }
                };
                if let Some(e) = err {
                    return Err(sql_err(vm, e));
                }
                if crate::dbg_flag("OVELO_DBDBG") {
                    eprintln!(
                        "[bundled] step h={h} -> {has} (rows={}, cur={:?})",
                        vm.bundled_stmts
                            .get(&(h as u64))
                            .map(|s| s.rows.len())
                            .unwrap_or(0),
                        vm.bundled_stmts.get(&(h as u64)).and_then(|s| s.cur)
                    );
                }
                Some(V::I32(has as i32))
            }
            "nativeReset" => {
                let h = i64v(args[0]);
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.cur = None;
                    s.executed = false;
                }
                None
            }
            "nativeClearBindings" => {
                let h = i64v(args[0]);
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.params.clear();
                }
                None
            }
            "nativeBindNull" => {
                let (h, idx) = (i64v(args[0]), i64v(args[1]).max(1) as usize);
                if crate::dbg_flag("OVELO_DBDBG") {
                    eprintln!("[bundled] bindNull h={h} idx={idx}");
                }
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.params.resize(s.params.len().max(idx), SV::Null);
                    // Batch use: the caller binds params for row N+1 over
                    // the same statement — a resize without the store
                    // leaves row N's value at this index.
                    s.params[idx - 1] = SV::Null;
                }
                None
            }
            "nativeBindLong" => {
                let (h, idx, v) = (i64v(args[0]), i64v(args[1]).max(1) as usize, i64v(args[2]));
                if crate::dbg_flag("OVELO_DBDBG") {
                    eprintln!("[bundled] bindLong h={h} idx={idx} v={v}");
                }
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.params.resize(s.params.len().max(idx), SV::Null);
                    s.params[idx - 1] = SV::Integer(v);
                }
                None
            }
            "nativeBindDouble" => {
                let (h, idx, v) = (i64v(args[0]), i64v(args[1]).max(1) as usize, f64v(args[2]));
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.params.resize(s.params.len().max(idx), SV::Null);
                    s.params[idx - 1] = SV::Real(v);
                }
                None
            }
            "nativeBindText" => {
                let (h, idx) = (i64v(args[0]), i64v(args[1]).max(1) as usize);
                if crate::dbg_flag("OVELO_DBDBG") {
                    eprintln!("[bundled] bindText h={h} idx={idx}");
                }
                let t = args.get(2).and_then(|v| sv(vm, *v)).unwrap_or_default();
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.params.resize(s.params.len().max(idx), SV::Null);
                    s.params[idx - 1] = SV::Text(t);
                }
                None
            }
            "nativeBindBlob" => {
                let (h, idx) = (i64v(args[0]), i64v(args[1]).max(1) as usize);
                let b = match args.get(2) {
                    Some(V::Ref(h)) => match vm.heap.get(*h) {
                        Some(Obj::Array { data, .. }) => {
                            data.iter().map(|b| i32v(*b) as u8).collect()
                        }
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                if let Some(s) = vm.bundled_stmts.get_mut(&(h as u64)) {
                    s.params.resize(s.params.len().max(idx), SV::Null);
                    s.params[idx - 1] = SV::Blob(b);
                }
                None
            }
            "nativeGetColumnCount" => {
                let h = i64v(args[0]);
                if !vm.bundled_stmts.contains_key(&(h as u64)) {
                    return Err(sql_err(vm, format!("statement {h} is closed")));
                }
                let (err, n) = {
                    let s = vm.bundled_stmts.get_mut(&(h as u64)).unwrap();
                    let err = if s.executed { None } else { s.exec().err() };
                    (err, s.cols.len() as i32)
                };
                if let Some(e) = err {
                    return Err(sql_err(vm, e));
                }
                Some(V::I32(n))
            }
            "nativeGetColumnName" => {
                let (h, col) = (i64v(args[0]), i64v(args[1]));
                if !vm.bundled_stmts.contains_key(&(h as u64)) {
                    return Err(sql_err(vm, format!("statement {h} is closed")));
                }
                let (err, n) = {
                    let s = vm.bundled_stmts.get_mut(&(h as u64)).unwrap();
                    let err = if s.executed { None } else { s.exec().err() };
                    (
                        err,
                        s.cols.get(col.max(0) as usize).cloned().unwrap_or_default(),
                    )
                };
                if let Some(e) = err {
                    return Err(sql_err(vm, e));
                }
                Some(jstr(vm, &n))
            }
            "nativeGetColumnType" => {
                let (h, col) = (i64v(args[0]), i64v(args[1]));
                let t = vm
                    .bundled_stmts
                    .get(&(h as u64))
                    .map(|s| sv_coltype(&s.cell(col)))
                    .unwrap_or(5);
                Some(V::I32(t))
            }
            "nativeGetLong" => {
                let (h, col) = (i64v(args[0]), i64v(args[1]));
                let v = vm
                    .bundled_stmts
                    .get(&(h as u64))
                    .map(|s| s.cell(col))
                    .unwrap_or(SV::Null);
                if crate::dbg_flag("OVELO_DBDBG") {
                    eprintln!("[bundled] getLong h={h} col={col} -> {:?}", v);
                }
                Some(V::I64(sv_long(&v)))
            }
            "nativeGetDouble" => {
                let (h, col) = (i64v(args[0]), i64v(args[1]));
                let v = vm
                    .bundled_stmts
                    .get(&(h as u64))
                    .map(|s| s.cell(col))
                    .unwrap_or(SV::Null);
                Some(V::F64(sv_double(&v)))
            }
            "nativeGetText" => {
                let (h, col) = (i64v(args[0]), i64v(args[1]));
                let v = vm
                    .bundled_stmts
                    .get(&(h as u64))
                    .map(|s| s.cell(col))
                    .unwrap_or(SV::Null);
                Some(match v {
                    SV::Null => V::Ref(0),
                    SV::Text(t) => jstr(vm, &t),
                    SV::Integer(i) => jstr(vm, &i.to_string()),
                    SV::Real(f) => jstr(vm, &f.to_string()),
                    SV::Blob(b) => jstr(vm, &String::from_utf8_lossy(&b)),
                })
            }
            "nativeGetBlob" => {
                let (h, col) = (i64v(args[0]), i64v(args[1]));
                let v = vm
                    .bundled_stmts
                    .get(&(h as u64))
                    .map(|s| s.cell(col))
                    .unwrap_or(SV::Null);
                Some(match v {
                    SV::Blob(b) => arr(vm, "B", b.iter().map(|&x| V::I32(x as i32)).collect()),
                    _ => V::Ref(0),
                })
            }
            _ => return Ok(None),
        })
    })();
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) => Err(e),
    }
}
