//! `android.content.Context`: package identity, private directories and
//! preferences, the way the runtime presents them to app code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::prefs::SharedPreferences;

/// A launched app's context: package-scoped directories and preference
/// stores, mirroring `ContextImpl`'s conventions (data_dir/<pkg>/{files,
/// cache, shared_prefs}).
pub struct Context {
    package_name: String,
    /// `<data>/<pkg>` — the app's private sandbox root on disk.
    data_dir: PathBuf,
    /// open preference stores by name (SharedPreferences are singletons
    /// per name per app, like `ContextImpl.sSharedPrefsCache`).
    prefs: HashMap<String, SharedPreferences>,
}

impl Context {
    /// Create the context for `package_name` rooted at `data_dir`.
    /// Directory layout matches Android: `files/`, `cache/`,
    /// `shared_prefs/<name>.xml`.
    pub fn new(package_name: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            package_name: package_name.into(),
            data_dir: data_dir.into(),
            prefs: HashMap::new(),
        }
    }

    /// `getPackageName()`.
    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    /// `getDataDir()`.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// `getFilesDir()` — `<data>/files`, created on demand like the
    /// platform's `ensurePrivateDirExists`.
    pub fn files_dir(&self) -> PathBuf {
        let d = self.data_dir.join("files");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// `getCacheDir()` — `<data>/cache`, created on demand.
    pub fn cache_dir(&self) -> PathBuf {
        let d = self.data_dir.join("cache");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// `getSharedPreferences(name, mode)` — one live store per name.
    ///
    /// The first call loads (or creates on commit) `shared_prefs/<name>.xml`.
    /// `name` is confined to the preferences directory: characters outside
    /// `[A-Za-z0-9._-]` are replaced with `_` so a malicious name can never
    /// escape the app's private directory.
    pub fn shared_prefs(&mut self, name: &str) -> &mut SharedPreferences {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = self
            .data_dir
            .join("shared_prefs")
            .join(format!("{safe}.xml"));
        self.prefs
            .entry(name.to_string())
            .or_insert_with(|| SharedPreferences::open(path))
    }
}
