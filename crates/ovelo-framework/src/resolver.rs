//! `ContentResolver`/`ContentProvider` in-app processing (design doc §08):
//! `content://authority/path` URIs dispatch to providers registered in the
//! process — no Binder driver or system_server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ovelo_error::OveloError;

use crate::err;
use crate::sqlite::{Cursor, SqlValue};

/// A parsed `content://` URI (`authority` + `/path` + `?query` + `#frag`).
#[derive(Debug, Clone, PartialEq)]
pub struct ContentUri {
    /// `content://<authority>/...`.
    pub authority: String,
    /// Path segments after the authority.
    pub path: Vec<String>,
    /// Raw query string, if present.
    pub query: Option<String>,
}

impl ContentUri {
    /// Parse `content://authority/path...`; other schemes are rejected.
    pub fn parse(uri: &str) -> Result<Self, OveloError> {
        let rest = uri
            .strip_prefix("content://")
            .ok_or_else(|| err("framework.uri.scheme").with_detail(uri))?;
        let (rest, _frag) = rest
            .split_once('#')
            .map_or((rest, None), |(r, f)| (r, Some(f)));
        let (rest, query) = rest
            .split_once('?')
            .map_or((rest, None), |(r, q)| (r, Some(q.to_string())));
        let mut it = rest.split('/');
        let authority = it.next().unwrap_or_default();
        if authority.is_empty() {
            return Err(err("framework.uri.authority").with_detail(uri));
        }
        Ok(Self {
            authority: authority.to_string(),
            path: it
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
            query,
        })
    }
}

/// In-process content provider (SQLite-backed ones call [`Cursor`]).
pub trait ContentProvider: Send {
    /// `query(uri, projection, selection, args, sortOrder)`.
    fn query(
        &mut self,
        uri: &ContentUri,
        projection: Option<&[&str]>,
        selection: Option<&str>,
        args: Option<&[&str]>,
        sort: Option<&str>,
    ) -> Result<Cursor, OveloError>;

    /// `insert(uri, values)` — returns the new row id.
    fn insert(
        &mut self,
        _uri: &ContentUri,
        _values: &[(&str, SqlValue)],
    ) -> Result<i64, OveloError> {
        Err(err("framework.provider.unsupported"))
    }

    /// `delete(uri, where, args)` — rows removed.
    fn delete(
        &mut self,
        _uri: &ContentUri,
        _clause: Option<&str>,
        _args: Option<&[&str]>,
    ) -> Result<usize, OveloError> {
        Err(err("framework.provider.unsupported"))
    }
}

/// `Context.getContentResolver()` — routes `content://` URIs to providers.
#[derive(Default)]
pub struct ContentResolver {
    providers: HashMap<String, Arc<Mutex<dyn ContentProvider>>>,
}

impl ContentResolver {
    /// Empty resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` for `authority`.
    pub fn register(&mut self, authority: &str, provider: Arc<Mutex<dyn ContentProvider>>) {
        self.providers.insert(authority.to_string(), provider);
    }

    fn provider(
        &self,
        uri: &str,
    ) -> Result<(ContentUri, Arc<Mutex<dyn ContentProvider>>), OveloError> {
        let u = ContentUri::parse(uri)?;
        let p = self
            .providers
            .get(&u.authority)
            .cloned()
            .ok_or_else(|| err("framework.provider.missing").with_detail(&u.authority))?;
        Ok((u, p))
    }

    /// `resolver.query(...)`.
    pub fn query(
        &self,
        uri: &str,
        projection: Option<&[&str]>,
        selection: Option<&str>,
        args: Option<&[&str]>,
        sort: Option<&str>,
    ) -> Result<Cursor, OveloError> {
        let (u, p) = self.provider(uri)?;
        let mut g = p.lock().map_err(|_| err("framework.provider.poison"))?;
        g.query(&u, projection, selection, args, sort)
    }

    /// `resolver.insert(...)`.
    pub fn insert(&self, uri: &str, values: &[(&str, SqlValue)]) -> Result<i64, OveloError> {
        let (u, p) = self.provider(uri)?;
        let mut g = p.lock().map_err(|_| err("framework.provider.poison"))?;
        g.insert(&u, values)
    }

    /// `resolver.delete(...)`.
    pub fn delete(
        &self,
        uri: &str,
        clause: Option<&str>,
        args: Option<&[&str]>,
    ) -> Result<usize, OveloError> {
        let (u, p) = self.provider(uri)?;
        let mut g = p.lock().map_err(|_| err("framework.provider.poison"))?;
        g.delete(&u, clause, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Database;

    /// Provider that maps a content authority onto a SQLite table.
    struct DbProvider {
        db: Database,
    }
    impl ContentProvider for DbProvider {
        fn query(
            &mut self,
            _u: &ContentUri,
            _p: Option<&[&str]>,
            sel: Option<&str>,
            args: Option<&[&str]>,
            _s: Option<&str>,
        ) -> Result<Cursor, OveloError> {
            let sql = match sel {
                Some(w) => format!("SELECT * FROM notes WHERE {w}"),
                None => "SELECT * FROM notes".to_string(),
            };
            self.db.query(&sql, args.unwrap_or(&[]))
        }
    }

    #[test]
    fn uri_parse() {
        let u = ContentUri::parse("content://com.x.notes/items/3?p=1#f").unwrap();
        assert_eq!(u.authority, "com.x.notes");
        assert_eq!(u.path, vec!["items", "3"]);
        assert_eq!(u.query.as_deref(), Some("p=1"));
        assert!(ContentUri::parse("http://x/").is_err());
        assert!(ContentUri::parse("content:///x").is_err());
    }

    #[test]
    fn dispatches_by_authority() {
        let db = Database::memory().unwrap();
        db.exec("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT)")
            .unwrap();
        db.insert("notes", &[("body", SqlValue::Text("one".into()))])
            .unwrap();
        db.insert("notes", &[("body", SqlValue::Text("two".into()))])
            .unwrap();

        let mut r = ContentResolver::new();
        r.register("com.x.notes", Arc::new(Mutex::new(DbProvider { db })));
        let c = r
            .query(
                "content://com.x.notes/items",
                None,
                Some("body = ?"),
                Some(&["two"]),
                None,
            )
            .unwrap();
        assert_eq!(c.count(), 1);
        assert!(r
            .query("content://other/x", None, None, None, None)
            .is_err());
    }
}
