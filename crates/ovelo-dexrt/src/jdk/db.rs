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
use crate::{E, Rt, Vm};

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

/// Open (or fetch) the connection for `name`; `:memory:` stays in RAM.
fn conn_for(vm: &mut Vm, name: &str) -> Result<Rc<RefCell<Db>>, E> {
    let name = safe_name(name);
    if let Some(d) = vm.dbs.get(&name) {
        return Ok(d.clone());
    }
    let conn = if name == ":memory:" {
        Connection::open_in_memory()
    } else {
        let dir = db_dir(vm);
        let _ = std::fs::create_dir_all(&dir);
        Connection::open(dir.join(&name))
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
    let d = Rc::new(RefCell::new(Db {
        conn,
        tx_stack: Vec::new(),
    }));
    vm.dbs.insert(name, d.clone());
    Ok(d)
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
    vm.throw_builtin(
        "Landroid/database/sqlite/SQLiteException;",
        &e.to_string(),
    )
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
        let cols: Vec<String> =
            stmt.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = stmt
            .query(params_from_iter(params))
            .map_err(|e| sql_err(vm, e))?;
        let mut cells = Vec::new();
        while let Some(r) = rows.next().map_err(|e| sql_err(vm, e))? {
            let mut row = Vec::with_capacity(cols.len());
            for i in 0..cols.len() {
                let v = r.get_ref(i).map_err(|e| sql_err(vm, e))?;
                row.push(match v {
                    ValueRef::Null => SV::Null,
                    ValueRef::Integer(i) => SV::Integer(i),
                    ValueRef::Real(f) => SV::Real(f),
                    ValueRef::Text(t) => {
                        SV::Text(String::from_utf8_lossy(t).to_string())
                    }
                    ValueRef::Blob(b) => SV::Blob(b.to_vec()),
                });
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
    let V::Ref(c) = host(vm, "Landroid/database/Cursor;", HostState::Empty)
    else {
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

/// Build `SELECT …` for `query*` overloads: scan args for
/// (table, columns[], selection, selectionArgs[], groupBy, having,
/// orderBy, limit) in order.
fn build_query(vm: &Vm, args: &[V]) -> Option<(String, V)> {
    let mut strings = Vec::new();
    let mut arrays = Vec::new();
    for a in &args[1..] {
        match a {
            V::Ref(0) => {}
            V::Ref(h)
                if matches!(vm.heap.get(*h), Some(Obj::Array { .. })) =>
            {
                arrays.push(*a)
            }
            V::Ref(h) if s(vm, *h).is_some() => {
                strings.push(s(vm, *h).unwrap_or_default())
            }
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
        .find(|a| {
            matches!(a, V::Ref(h) if matches!(vm.heap.get(*h), Some(Obj::Array{..})))
        })
        .copied()
        .unwrap_or(V::Ref(0))
}

/// Single entry: `android.database.*` + `androidx.sqlite` receivers.
/// Routes by receiver shape — db-bound objects get sqlite ops,
/// P_SQL-carrying ones get statement ops, everything else cursor ops.
pub(crate) fn db_call(
    vm: &mut Vm,
    name: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    let h = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    if std::env::var_os("OVELO_DBDBG").is_some() {
        eprintln!("[db] {name}(h={h})");
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
fn sqlite_call(
    vm: &mut Vm,
    name: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
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
                            .query_row::<i64, _, _>(
                                "PRAGMA user_version",
                                [],
                                |r| r.get(0),
                            )
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
                    vm.call_virtual(this, hook, &cb_args).map_err(E::Vm)?;
                    if let Ok(db) = conn_of(vm, this) {
                        let _ = db
                            .borrow()
                            .conn
                            .execute_batch(&format!("PRAGMA user_version={want}"));
                    }
                }
            }
            Some(Some(out))
        }
        "beginTransaction" | "beginTransactionNonExclusive"
        | "beginTransactionWithListener"
        | "beginTransactionWithListenerNonExclusive" => {
            let db = conn_of(vm, this)?;
            let mut d = db.borrow_mut();
            // Nested begins stack on one level each — only the
            // outermost pair issues BEGIN/COMMIT/ROLLBACK.
            if d.tx_stack.is_empty() {
                d.conn
                    .execute_batch("BEGIN")
                    .map_err(|e| sql_err(vm, e))?;
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
                // transaction poisons the outer one's outcome.
                *top &= ok;
            } else {
                d.conn
                    .execute_batch(if ok { "COMMIT" } else { "ROLLBACK" })
                    .map_err(|e| sql_err(vm, e))?;
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
            let binds = find_array_arg(vm, &args[2..]);
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
        "insert" | "insertOrThrow" | "insertWithOnConflict" | "replace"
        | "replaceOrThrow" => {
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
                let qs = pairs
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");
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
            let mut params: Vec<SV> =
                pairs.iter().map(|(_, v)| v_to_sql(vm, *v)).collect();
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
                .execute(&sql, params_from_iter(binds_of(vm, find_array_arg(vm, args))))
                .map_err(|e| sql_err(vm, e))?;
            Some(Some(V::I32(n as i32)))
        }
        "isOpen" => Some(Some(V::I32(1))),
        "close" => {
            if let Some(n) = db_name_of(vm, this) {
                vm.dbs.remove(&n);
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
            Some(Some(jstr(
                vm,
                &format!("/data/local/ovelo/databases/{n}"),
            )))
        }
        "getMaximumSize" | "getPageSize" => Some(Some(V::I64(0))),
        "isReadOnly" | "isDbLockedByCurrentThread"
        | "isDbLockedByOtherThreads" | "isWriteAheadLoggingEnabled" => {
            Some(Some(V::I32(0)))
        }
        "isDatabaseIntegrityOk" | "yieldIfContendedSafely" => {
            Some(Some(V::I32(1)))
        }
        "setForeignKeyConstraintsEnabled" | "enableWriteAheadLogging"
        | "disableWriteAheadLogging" | "setMaximumSize" | "setPageSize"
        | "setIdleConnectionTimeout" | "setLocale" | "setLockingEnabled"
        | "acquireReference" | "releaseReference" | "releaseMemory"
        | "setWriteAheadLoggingEnabled" | "markTableSyncable" => Some(None),
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
                let idx =
                    i32v(*args.get(1).unwrap_or(&V::I32(0))) as usize;
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
                .query_row(&sql, params_from_iter(params), |r| {
                    r.get::<_, String>(0)
                })
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
            Some(Obj::Array { data, .. }) => {
                data.iter().filter_map(|v| sv(vm, *v)).collect()
            }
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
fn cursor_call(
    vm: &mut Vm,
    name: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
        return Ok(None);
    };
    let n_rows = cur_rows(vm, h).len() as i32;
    let col_arg = i32v(*args.get(1).unwrap_or(&V::I32(0)));
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
        "isFirst" => {
            Some(Some(V::I32((cur_pos(vm, h) == 0 && n_rows > 0) as i32)))
        }
        "isLast" => Some(Some(V::I32(
            (cur_pos(vm, h) == n_rows - 1 && n_rows > 0) as i32,
        ))),
        "isBeforeFirst" => {
            Some(Some(V::I32((cur_pos(vm, h) < 0 && n_rows > 0) as i32)))
        }
        "isAfterLast" => {
            Some(Some(V::I32((cur_pos(vm, h) >= n_rows && n_rows > 0) as i32)))
        }
        "getColumnIndex" | "getColumnIndexOrThrow" => {
            let want =
                sv(vm, *args.get(1).unwrap_or(&V::Ref(0))).unwrap_or_default();
            let cols = cur_cols(vm, h);
            let idx = cols
                .iter()
                .position(|c| c == &want)
                .or_else(|| {
                    cols.iter().position(|c| c.eq_ignore_ascii_case(&want))
                })
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
            let vs: Vec<V> =
                cur_cols(vm, h).iter().map(|c| jstr(vm, c)).collect();
            Some(Some(arr(vm, STRING, vs)))
        }
        "getColumnCount" => Some(Some(V::I32(cur_cols(vm, h).len() as i32))),
        "getString" => {
            let v = cur_cell(vm, h, col_arg);
            Some(Some(match v {
                V::Ref(_) => v,
                V::I32(i) => jstr(vm, &i.to_string()),
                V::I64(i) => jstr(vm, &i.to_string()),
                V::F32(f) => jstr(vm, &f.to_string()),
                V::F64(f) => jstr(vm, &f.to_string()),
            }))
        }
        "getInt" | "getShort" => {
            Some(Some(V::I32(i64v(cur_cell(vm, h, col_arg)) as i32)))
        }
        "getLong" => Some(Some(V::I64(i64v(cur_cell(vm, h, col_arg))))),
        "getFloat" => {
            Some(Some(V::F32(f64v(cur_cell(vm, h, col_arg)) as f32)))
        }
        "getDouble" => Some(Some(V::F64(f64v(cur_cell(vm, h, col_arg))))),
        "getBlob" => Some(Some(cur_cell(vm, h, col_arg))),
        "isNull" => Some(Some(V::I32(
            matches!(cur_cell(vm, h, col_arg), V::Ref(0)) as i32,
        ))),
        "getType" => Some(Some(V::I32(match cur_cell(vm, h, col_arg) {
            V::Ref(0) => 0,
            V::I32(_) | V::I64(_) => 1,
            V::F32(_) | V::F64(_) => 2,
            V::Ref(rh) if s(vm, rh).is_some() => 3,
            _ => 4,
        }))),
        "getExtras" | "getNotificationUri" | "respond" => {
            Some(Some(V::Ref(0)))
        }
        "requery" => Some(Some(V::I32(0))),
        "getWantsAllOnMoveCalls" => Some(Some(V::I32(0))),
        "copyStringToBuffer" | "close" | "deactivate"
        | "registerContentObserver" | "unregisterContentObserver"
        | "registerDataSetObserver" | "unregisterDataSetObserver"
        | "setNotificationUri" | "setExtras" => Some(None),
        _ => None,
    })
}
