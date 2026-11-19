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

    /// `getFilesDir()` — `<data>/files`.
    pub fn files_dir(&self) -> PathBuf {
        self.data_dir.join("files")
    }

    /// `getCacheDir()` — `<data>/cache`.
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }

    /// `getSharedPreferences(name, mode)` — one live store per name.
    ///
    /// The first call loads (or creates on commit) `shared_prefs/<name>.xml`.
    pub fn shared_prefs(&mut self, name: &str) -> &mut SharedPreferences {
        let path = self.data_dir.join("shared_prefs").join(format!("{name}.xml"));
        self.prefs
            .entry(name.to_string())
            .or_insert_with(|| SharedPreferences::open(path))
    }
}
