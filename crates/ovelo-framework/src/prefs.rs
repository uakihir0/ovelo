//! `SharedPreferences` with AOSP-compatible XML persistence.
//!
//! Documents use the platform's `<map>` schema:
//! `<map><int name="k" value="1"/><string name="s">v</string>
//!  <boolean name="b" value="true"/><long name="l" value="7"/>
//!  <float name="f" value="1.5"/>
//!  <set name="ss"><string>v1</string><string>v2</string></set></map>`
//!
//! `commit()` writes synchronously and reports success; `apply()` stages the
//! write and returns immediately (the store writes on the next `sync()`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use crate::err;
use ovelo_error::OveloError;

/// A preference value; mirrors the typed getters on `SharedPreferences`.
#[derive(Debug, Clone, PartialEq)]
pub enum PrefValue {
    /// `String` preference.
    Str(String),
    /// `int` preference.
    Int(i32),
    /// `long` preference.
    Long(i64),
    /// `float` preference.
    Float(f32),
    /// `boolean` preference.
    Bool(bool),
    /// `Set<String>` preference.
    StrSet(HashSet<String>),
}

/// Maximum prefs file size we will read — far above any real usage; a
/// tampered or corrupt oversized file is treated as absent rather than
/// exhausting the worker's memory.
const MAX_PREFS_FILE: u64 = 4 * 1024 * 1024;
/// Maximum entries parsed from one document.
const MAX_PREFS_ENTRIES: usize = 10_000;

/// An editor-backed key/value store with XML persistence.
pub struct SharedPreferences {
    path: PathBuf,
    map: HashMap<String, PrefValue>,
    /// Staged by `apply()`; written on the next `commit()`/`sync()`.
    dirty: bool,
    /// Per-instance id making this store's temp file name unique.
    tmp_id: u64,
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl SharedPreferences {
    /// Open (or lazily create) the store at `path`.
    pub fn open(path: PathBuf) -> Self {
        let map = match std::fs::metadata(&path) {
            Ok(m) if m.len() > MAX_PREFS_FILE => HashMap::new(),
            Ok(_) => std::fs::read_to_string(&path)
                .ok()
                .map(|s| parse_map(&s))
                .unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            map,
            dirty: false,
            tmp_id: TMP_SEQ.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Backing file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `contains(key)`.
    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// `getString(key, def)` — wrong type returns `def`, like the platform.
    pub fn get_string<'a>(&'a self, key: &str, def: &'a str) -> &'a str {
        match self.map.get(key) {
            Some(PrefValue::Str(s)) => s,
            _ => def,
        }
    }

    /// `getInt(key, def)`.
    pub fn get_int(&self, key: &str, def: i32) -> i32 {
        match self.map.get(key) {
            Some(PrefValue::Int(i)) => *i,
            _ => def,
        }
    }

    /// `getLong(key, def)`.
    pub fn get_long(&self, key: &str, def: i64) -> i64 {
        match self.map.get(key) {
            Some(PrefValue::Long(l)) => *l,
            _ => def,
        }
    }

    /// `getFloat(key, def)`.
    pub fn get_float(&self, key: &str, def: f32) -> f32 {
        match self.map.get(key) {
            Some(PrefValue::Float(f)) => *f,
            _ => def,
        }
    }

    /// `getBoolean(key, def)`.
    pub fn get_bool(&self, key: &str, def: bool) -> bool {
        match self.map.get(key) {
            Some(PrefValue::Bool(b)) => *b,
            _ => def,
        }
    }

    /// `getStringSet(key, def)` — returns a copy (the live set is not shared).
    pub fn get_string_set(&self, key: &str) -> Option<HashSet<String>> {
        match self.map.get(key) {
            Some(PrefValue::StrSet(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// `getAll()` — snapshot of every entry.
    pub fn get_all(&self) -> &HashMap<String, PrefValue> {
        &self.map
    }

    /// `Editor.put*` — stage a value.
    pub fn put(&mut self, key: impl Into<String>, v: PrefValue) {
        self.map.insert(key.into(), v);
    }

    /// `Editor.remove(key)`.
    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }

    /// `Editor.clear()`.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// `Editor.commit()` — write through, return success. On failure the
    /// dirty flag is kept so a later `sync()` retries the write.
    pub fn commit(&mut self) -> Result<bool, OveloError> {
        match self.write() {
            Ok(()) => {
                self.dirty = false;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    /// `Editor.apply()` — stage the write, return immediately.
    pub fn apply(&mut self) {
        self.dirty = true;
    }

    /// Flush any `apply()`ed changes.
    pub fn sync(&mut self) -> Result<bool, OveloError> {
        if self.dirty {
            self.commit()
        } else {
            Ok(true)
        }
    }

    fn write(&self) -> Result<(), OveloError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| err("framework.prefs.io").with_detail(e.to_string()))?;
        }
        if self.map.len() > MAX_PREFS_ENTRIES {
            return Err(err("framework.prefs.too_large"));
        }
        let doc = write_map(&self.map);
        if doc.len() as u64 > MAX_PREFS_FILE {
            return Err(err("framework.prefs.too_large"));
        }
        // Write to a sibling temp file and rename so a failed write never
        // truncates the existing document (like AOSP's .bak handling).
        // The temp name carries this instance's id: two stores opened on
        // sibling paths (settings.xml vs settings.bak) would otherwise
        // share `settings.xml.tmp` and corrupt each other's writes.
        let tmp = self.path.with_extension(format!("xml.tmp-{}", self.tmp_id));
        std::fs::write(&tmp, doc)
            .map_err(|e| err("framework.prefs.io").with_detail(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err("framework.prefs.io").with_detail(e.to_string())
        })
    }
}

/// Process-wide store cache, like `ContextImpl.sSharedPrefsCache`: one live
/// store per backing file, shared by every `Context` in the app.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<SharedPreferences>>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<SharedPreferences>>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get (or lazily open) the shared store for `path`.
pub fn shared(path: PathBuf) -> Arc<Mutex<SharedPreferences>> {
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    r.entry(path.clone())
        .or_insert_with(|| Arc::new(Mutex::new(SharedPreferences::open(path))))
        .clone()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn unesc(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn write_map(map: &HashMap<String, PrefValue>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort(); // deterministic output
    let mut out = String::from("<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n<map>\n");
    for k in keys {
        let n = esc(k);
        match &map[k] {
            PrefValue::Str(v) => {
                out.push_str(&format!("    <string name=\"{n}\">{}</string>\n", esc(v)));
            }
            PrefValue::Int(v) => {
                out.push_str(&format!("    <int name=\"{n}\" value=\"{v}\"/>\n"));
            }
            PrefValue::Long(v) => {
                out.push_str(&format!("    <long name=\"{n}\" value=\"{v}\"/>\n"));
            }
            PrefValue::Float(v) => {
                out.push_str(&format!("    <float name=\"{n}\" value=\"{v}\"/>\n"));
            }
            PrefValue::Bool(v) => {
                out.push_str(&format!("    <boolean name=\"{n}\" value=\"{v}\"/>\n"));
            }
            PrefValue::StrSet(v) => {
                out.push_str(&format!("    <set name=\"{n}\">\n"));
                let mut items: Vec<&String> = v.iter().collect();
                items.sort();
                for s in items {
                    out.push_str(&format!("        <string>{}</string>\n", esc(s)));
                }
                out.push_str("    </set>\n");
            }
        }
    }
    out.push_str("</map>\n");
    out
}

/// Parse the platform `<map>` schema (attribute-free tokenizer; our writer
/// and AOSP's both emit this exact shape).
fn parse_map(s: &str) -> HashMap<String, PrefValue> {
    let mut out = HashMap::new();
    let mut rest = s;
    let mut entries = 0usize;
    while let Some(i) = rest.find('<') {
        let end = match rest[i..].find('>') {
            Some(j) => i + j,
            None => break,
        };
        let tag = &rest[i + 1..end];
        let tag_body = tag.trim_end_matches('/');
        let close = tag.starts_with('/');
        if close {
            rest = &rest[end + 1..];
            continue;
        }
        let mut it = tag_body.split_whitespace();
        let name = it.next().unwrap_or("");
        match name {
            "int" | "long" | "float" | "boolean" => {
                entries += 1;
                if entries > MAX_PREFS_ENTRIES {
                    break;
                }
                if let (Some(key), Some(val)) = (attr(tag_body, "name"), attr(tag_body, "value")) {
                    let v = match name {
                        "int" => val.parse::<i32>().ok().map(PrefValue::Int),
                        "long" => val.parse::<i64>().ok().map(PrefValue::Long),
                        "float" => val.parse::<f32>().ok().map(PrefValue::Float),
                        _ => Some(PrefValue::Bool(val == "true")),
                    };
                    if let Some(v) = v {
                        out.insert(unesc(&key), v);
                    }
                }
            }
            "string" => {
                entries += 1;
                if entries > MAX_PREFS_ENTRIES {
                    break;
                }
                if let Some(key) = attr(tag_body, "name") {
                    // <string name="k">text</string>
                    let text_start = end + 1;
                    let close = rest[text_start..].find("</string>");
                    if let Some(c) = close {
                        let text = &rest[text_start..text_start + c];
                        out.insert(unesc(&key), PrefValue::Str(unesc(text)));
                        rest = &rest[text_start + c..];
                        continue;
                    }
                }
            }
            "set" => {
                entries += 1;
                if entries > MAX_PREFS_ENTRIES {
                    break;
                }
                if let Some(key) = attr(tag_body, "name") {
                    let text_start = end + 1;
                    if let Some(c) = rest[text_start..].find("</set>") {
                        let body = &rest[text_start..text_start + c];
                        let mut set = HashSet::new();
                        let mut r = body;
                        while let Some(si) = r.find("<string>") {
                            let from = si + "<string>".len();
                            if let Some(e) = r[from..].find("</string>") {
                                set.insert(unesc(&r[from..from + e]));
                                r = &r[from + e..];
                            } else {
                                break;
                            }
                        }
                        out.insert(unesc(&key), PrefValue::StrSet(set));
                        rest = &rest[text_start + c..];
                        continue;
                    }
                }
            }
            _ => {}
        }
        rest = &rest[end + 1..];
    }
    out
}

fn attr(tag: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let i = tag.find(&pat)? + pat.len();
    let j = tag[i..].find('"')? + i;
    Some(tag[i..j].to_string())
}
