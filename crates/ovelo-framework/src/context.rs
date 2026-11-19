//! `android.content.Context`: package identity, private directories and
//! preferences, the way the runtime presents them to app code.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ovelo_error::OveloError;

use crate::err;
use crate::prefs::{self, SharedPreferences};

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A launched app's context: package-scoped directories and preference
/// stores, mirroring `ContextImpl`'s conventions (data_dir/<pkg>/{files,
/// cache, shared_prefs}).
pub struct Context {
    package_name: String,
    /// `<data>/<pkg>` — the app's private sandbox root on disk.
    data_dir: PathBuf,
}

impl Context {
    /// Create the context for `package_name` rooted at `data_dir`.
    /// Directory layout matches Android: `files/`, `cache/`,
    /// `shared_prefs/<name>.xml`.
    pub fn new(package_name: impl Into<String>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            package_name: package_name.into(),
            data_dir: data_dir.into(),
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

    /// `openOrCreateDatabase(name)` — SQLite file under `databases/`.
    pub fn database(&self, name: &str) -> Result<crate::sqlite::Database, OveloError> {
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            || name.is_empty()
        {
            return Err(err("framework.db.name").with_detail(name));
        }
        crate::sqlite::Database::open(&self.data_dir.join("databases").join(format!("{name}.db")))
    }

    /// Instance-state file (`onSaveInstanceState` survives process death).
    fn state_path(&self) -> PathBuf {
        self.data_dir.join("state").join("instance_state.ovb")
    }

    /// Persist `onSaveInstanceState`'s bundle (atomic write).
    pub fn save_instance_state(&self, b: &crate::bundle::Bundle) -> Result<(), OveloError> {
        let p = self.state_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)
                .map_err(|e| err("framework.state.io").with_detail(e.to_string()))?;
        }
        // Unique tmp per save — concurrent saves must not share the
        // staging file or one writer can corrupt the other's published file.
        let tmp = p.with_extension(format!(
            "ovb.tmp-{}",
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&tmp, b.to_bytes())
            .map_err(|e| err("framework.state.io").with_detail(e.to_string()))?;
        std::fs::rename(&tmp, &p).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err("framework.state.io").with_detail(e.to_string())
        })
    }

    /// The bundle from the last `save_instance_state`, if any.
    /// A corrupt file is ignored — a fresh launch beats a crash loop.
    pub fn load_instance_state(&self) -> Option<crate::bundle::Bundle> {
        let raw = std::fs::read(self.state_path()).ok()?;
        crate::bundle::Bundle::from_bytes(&raw).ok()
    }

    /// Drop saved instance state (fresh install / wipe).
    pub fn clear_instance_state(&self) {
        let _ = std::fs::remove_file(self.state_path());
    }

    /// `getSharedPreferences(name, mode)` — one live store per name.
    ///
    /// The first call loads (or creates on commit) `shared_prefs/<name>.xml`.
    /// `name` is confined to the preferences directory: names already in
    /// `[A-Za-z0-9._-]` pass through, anything else is hex-encoded, so a
    /// malicious name can never escape the app's private directory.
    /// Returns the process-wide shared store (matches AOSP's singleton
    /// per file), so writes via one `Context` are visible to all others.
    pub fn shared_prefs(&mut self, name: &str) -> Arc<Mutex<SharedPreferences>> {
        // Valid names pass through unchanged (so files written before
        // sanitisation are still found); anything else is hex-encoded —
        // a lossless escape, so distinct names can never collide.
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
        let safe: String = if valid {
            name.to_string()
        } else {
            name.bytes().map(|b| format!("{b:02x}")).collect()
        };
        let path = self
            .data_dir
            .join("shared_prefs")
            .join(format!("{safe}.xml"));
        prefs::shared(path)
    }
}
