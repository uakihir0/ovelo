//! `android.database.sqlite.SQLiteDatabase` subset backed by real SQLite
//! (design doc §08 "in-app processing for SQLite"). Query results are
//! materialized into a [`Cursor`] — enough for ContentProvider-style use.

use std::path::Path;

use ovelo_error::OveloError;
use rusqlite::{params_from_iter, Connection};

use crate::err;

/// A column value (`Cursor.get*`).
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    /// NULL.
    Null,
    /// INTEGER.
    Int(i64),
    /// REAL.
    Real(f64),
    /// TEXT.
    Text(String),
    /// BLOB.
    Blob(Vec<u8>),
}

/// Materialized query result (`android.database.Cursor`).
#[derive(Debug, Clone, Default)]
pub struct Cursor {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Rows; each row has `columns.len()` values.
    pub rows: Vec<Vec<SqlValue>>,
    /// Current position (-1 = before first, like a fresh cursor).
    pos: isize,
}

impl Cursor {
    /// `getCount()`.
    pub fn count(&self) -> usize {
        self.rows.len()
    }
    /// `moveToNext()`.
    pub fn move_next(&mut self) -> bool {
        if self.pos + 1 < self.rows.len() as isize {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    /// `moveToFirst()`.
    pub fn first(&mut self) -> bool {
        if self.rows.is_empty() {
            false
        } else {
            self.pos = 0;
            true
        }
    }
    /// `getColumnIndex(name)` (-1 when absent).
    pub fn column_index(&self, name: &str) -> i32 {
        self.columns
            .iter()
            .position(|c| c == name)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }
    /// Value at the current row / column index.
    pub fn get(&self, col: usize) -> Option<&SqlValue> {
        if self.pos < 0 {
            return None;
        }
        self.rows.get(self.pos as usize)?.get(col)
    }
}

/// An open database.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// `openOrCreateDatabase(path)` — creates parent dirs and the file.
    pub fn open(path: &Path) -> Result<Self, OveloError> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)
                .map_err(|e| err("framework.db.io").with_detail(e.to_string()))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| err("framework.db.open").with_detail(e.to_string()))?;
        Ok(Self { conn })
    }

    /// In-memory database (`SQLiteDatabase.create(null)`).
    pub fn memory() -> Result<Self, OveloError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| err("framework.db.open").with_detail(e.to_string()))?;
        Ok(Self { conn })
    }

    /// `execSQL` — no result rows (DDL/DML).
    pub fn exec(&self, sql: &str) -> Result<(), OveloError> {
        self.conn
            .execute_batch(sql)
            .map_err(|e| err("framework.db.exec").with_detail(e.to_string()))
    }

    /// `rawQuery` — returns a materialized cursor.
    pub fn query(&self, sql: &str, args: &[&str]) -> Result<Cursor, OveloError> {
        let mut st = self
            .conn
            .prepare(sql)
            .map_err(|e| err("framework.db.query").with_detail(e.to_string()))?;
        let columns: Vec<String> =
            st.column_names().iter().map(|s| s.to_string()).collect();
        let n = columns.len();
        let mut rows = Vec::new();
        let mut it = st
            .query(params_from_iter(args.iter()))
            .map_err(|e| err("framework.db.query").with_detail(e.to_string()))?;
        while let Some(r) = it
            .next()
            .map_err(|e| err("framework.db.query").with_detail(e.to_string()))?
        {
            let mut row = Vec::with_capacity(n);
            for i in 0..n {
                let v = r
                    .get_ref(i)
                    .map_err(|e| err("framework.db.query").with_detail(e.to_string()))?;
                row.push(match v {
                    rusqlite::types::ValueRef::Null => SqlValue::Null,
                    rusqlite::types::ValueRef::Integer(x) => SqlValue::Int(x),
                    rusqlite::types::ValueRef::Real(x) => SqlValue::Real(x),
                    rusqlite::types::ValueRef::Text(t) => {
                        SqlValue::Text(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
                });
            }
            rows.push(row);
        }
        Ok(Cursor {
            columns,
            rows,
            pos: -1,
        })
    }

    /// `insert(table, values)` — the new rowid, or -1 on failure like
    /// `SQLiteDatabase.insert` (never the stale `last_insert_rowid`).
    pub fn insert(&self, table: &str, values: &[(&str, SqlValue)]) -> Result<i64, OveloError> {
        let cols: Vec<&str> = values.iter().map(|(c, _)| *c).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            ident(table)?,
            cols.iter().map(|c| ident(c)).collect::<Result<Vec<_>, _>>()?.join(", "),
            cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ")
        );
        match self.conn.execute(
            &sql,
            params_from_iter(values.iter().map(|(_, v)| to_rv(v))),
        ) {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(_) => Ok(-1),
        }
    }

    /// `delete(table, where, args)` — rows deleted.
    pub fn delete(&self, table: &str, clause: &str, args: &[&str]) -> Result<usize, OveloError> {
        let sql = format!("DELETE FROM {} WHERE {clause}", ident(table)?);
        self.conn
            .execute(&sql, params_from_iter(args.iter()))
            .map_err(|e| err("framework.db.exec").with_detail(e.to_string()))
    }

    /// `beginTransaction`.
    pub fn begin(&self) -> Result<(), OveloError> {
        self.exec("BEGIN")
    }
    /// `setTransactionSuccessful` + `endTransaction` → COMMIT.
    pub fn commit(&self) -> Result<(), OveloError> {
        self.exec("COMMIT")
    }
    /// `endTransaction` without success → ROLLBACK.
    pub fn rollback(&self) -> Result<(), OveloError> {
        self.exec("ROLLBACK")
    }
}

/// Validated identifier, double-quoted for SQL — `order`, `select` and
/// other keywords are legal table/column names once quoted.
fn ident(s: &str) -> Result<String, OveloError> {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.is_empty()
    {
        Ok(format!("\"{s}\""))
    } else {
        Err(err("framework.db.ident"))
    }
}

fn to_rv(v: &SqlValue) -> rusqlite::types::Value {
    match v {
        SqlValue::Null => rusqlite::types::Value::Null,
        SqlValue::Int(x) => rusqlite::types::Value::Integer(*x),
        SqlValue::Real(x) => rusqlite::types::Value::Real(*x),
        SqlValue::Text(s) => rusqlite::types::Value::Text(s.clone()),
        SqlValue::Blob(b) => rusqlite::types::Value::Blob(b.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_and_tx() {
        let db = Database::memory().unwrap();
        db.exec("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT)").unwrap();
        let id = db
            .insert("notes", &[("body", SqlValue::Text("hi".into()))])
            .unwrap();
        assert_eq!(id, 1);
        let mut c = db.query("SELECT id, body FROM notes", &[]).unwrap();
        assert_eq!(c.count(), 1);
        assert_eq!(c.column_index("body"), 1);
        assert!(c.first());
        assert_eq!(c.get(1), Some(&SqlValue::Text("hi".into())));

        db.begin().unwrap();
        db.insert("notes", &[("body", SqlValue::Text("drop".into()))]).unwrap();
        db.rollback().unwrap();
        assert_eq!(db.query("SELECT * FROM notes", &[]).unwrap().count(), 1);

        assert_eq!(
            db.delete("notes", "id = ?", &[&id.to_string()]).unwrap(),
            1
        );
    }

    #[test]
    fn rejects_bad_ident() {
        let db = Database::memory().unwrap();
        assert!(db.insert("a;b", &[]).is_err());
    }

    #[test]
    fn keyword_identifiers_and_conflict_minus_one() {
        let db = Database::memory().unwrap();
        // `order`/`select` are SQLite keywords but legal names once quoted.
        db.exec("CREATE TABLE \"order\" (\"select\" INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        let id = db
            .insert("order", &[("v", SqlValue::Text("a".into()))])
            .unwrap();
        assert_eq!(id, 1);
        // A failed insert returns -1, not the stale last rowid.
        db.exec("CREATE UNIQUE INDEX u ON \"order\"(v)").unwrap();
        assert_eq!(
            db.insert("order", &[("v", SqlValue::Text("a".into()))])
                .unwrap(),
            -1
        );
    }
}
