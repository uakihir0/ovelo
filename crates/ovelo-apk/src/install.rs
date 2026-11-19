//! Install records and content-hash storage (design doc §03 step 5).
//!
//! Layout under `store_root`:
//! - `apks/<sha256>` — original APK bytes by content hash (never rewritten)
//! - `derived/<sha256>/` — resource index / AOT cache for that APK
//! - `apps/<package>@<signer>/current` — version record, switched atomically
//!
//! Updates to an existing package+signer identity require the same signer;
//! a failed install keeps the previously active version.

use std::path::{Path, PathBuf};

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use sha2::{Digest, Sha256};

fn err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

fn io_err(op: &str, e: std::io::Error) -> OveloError {
    OveloError::new(ErrorCategory::HostCrash, Phase::Install, format!("install.io.{op}"))
        .with_detail(e.to_string())
}

/// The installed-app identity: package name + signer fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppId {
    /// Android package name.
    pub package: String,
    /// Canonical signer fingerprint: sorted SHA-256 cert hashes joined by `+`
    /// (one element for single-signer APKs).
    pub signer: String,
}

impl AppId {
    fn dir_name(&self) -> String {
        let pkg: String = self
            .package
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
            .collect();
        if self.signer.contains('+') {
            // Multi-signer: hash so distinct signer sets cannot collide.
            use sha2::Digest;
            let h: String = sha2::Sha256::digest(self.signer.as_bytes())
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{pkg}@{h}")
        } else {
            // Legacy layout for single signers (must not change: existing
            // installs are reachable only through this name).
            format!("{pkg}@{}", &self.signer[..16.min(self.signer.len())])
        }
    }
}

/// Record describing one installed version.
#[derive(Debug, Clone)]
pub struct InstallRecord {
    /// App identity.
    pub app: AppId,
    /// versionCode from the manifest.
    pub version_code: i64,
    /// SHA-256 of the stored APK bytes.
    pub apk_sha256: String,
    /// Package-relative path of the active APK in `apks/`.
    pub apk_path: PathBuf,
    /// Directory for derived artifacts (resource index, AOT cache).
    pub derived_dir: PathBuf,
    /// Unix seconds when this version was committed.
    pub installed_at: u64,
}

/// On-disk install store.
#[derive(Debug, Clone)]
pub struct InstallStore {
    root: PathBuf,
}

impl InstallStore {
    /// Open (or create) a store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for d in ["apks", "derived", "apps"] {
            std::fs::create_dir_all(root.join(d)).map_err(|e| io_err("mkdir", e))?;
        }
        Ok(Self { root })
    }

    /// Store APK bytes by content hash; returns the sha256 hex.
    pub fn store_apk(&self, bytes: &[u8]) -> Result<String> {
        let hash = Sha256::digest(bytes);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let dst = self.root.join("apks").join(&hex);
        if !dst.exists() {
            let tmp = self.root.join("apks").join(format!("{hex}.tmp"));
            std::fs::write(&tmp, bytes).map_err(|e| io_err("write_apk", e))?;
            std::fs::rename(&tmp, &dst).map_err(|e| io_err("rename_apk", e))?;
        }
        Ok(hex)
    }

    /// Derived-artifacts directory for an APK hash (created on demand).
    pub fn derived_dir(&self, apk_sha256: &str) -> Result<PathBuf> {
        let d = self.root.join("derived").join(apk_sha256);
        std::fs::create_dir_all(&d).map_err(|e| io_err("mkdir_derived", e))?;
        Ok(d)
    }

    /// Current install record for `app`, if any.
    pub fn current(&self, app: &AppId) -> Result<Option<InstallRecord>> {
        let cur = self.app_dir(app).join("current");
        if !cur.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&cur).map_err(|e| io_err("read_current", e))?;
        Ok(Some(parse_record(&text, &self.root)?))
    }

    /// Commit `apk_sha256` as the active version for `app`.
    ///
    /// `signer` is the verified signer fingerprint of this APK. When a record
    /// already exists it must have the same signer; a different signer is a
    /// `PKG_SIGNATURE` rejection and the previous version stays active.
    pub fn commit(&self, app: &AppId, version_code: i64, apk_sha256: &str, signer: &str) -> Result<InstallRecord> {
        if app.signer != signer {
            return Err(OveloError::new(
                ErrorCategory::PkgSignature,
                Phase::Install,
                "install.signer.mismatch",
            ));
        }
        if let Some(cur) = self.current(app)? {
            if cur.app.signer != signer {
                return Err(OveloError::new(
                    ErrorCategory::PkgSignature,
                    Phase::Install,
                    "install.signer.changed",
                ));
            }
        }
        let record = InstallRecord {
            app: app.clone(),
            version_code,
            apk_sha256: apk_sha256.to_string(),
            apk_path: self.root.join("apks").join(apk_sha256),
            derived_dir: self.derived_dir(apk_sha256)?,
            installed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        if !record.apk_path.exists() {
            return Err(err("install.apk.absent"));
        }
        let dir = self.app_dir(app);
        std::fs::create_dir_all(&dir).map_err(|e| io_err("mkdir_app", e))?;
        // Unique tmp name per call: concurrent commits must never share a path.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = dir.join(format!(
            "current.tmp.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let cur = dir.join("current");
        if let Err(e) = std::fs::write(&tmp, render_record(&record)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err("write_record", e));
        }
        // Atomic switch of the active-version reference.
        if let Err(e) = std::fs::rename(&tmp, &cur) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err("rename_record", e));
        }
        Ok(record)
    }

    /// Read stored APK bytes for a record.
    pub fn apk_bytes(&self, record: &InstallRecord) -> Result<Vec<u8>> {
        std::fs::read(&record.apk_path).map_err(|e| io_err("read_apk", e))
    }

    /// Full removal of an app's records (apk bytes stay — content-addressed,
    /// shared with other apps).
    pub fn remove(&self, app: &AppId) -> Result<()> {
        let dir = self.app_dir(app);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| io_err("remove_app", e))?;
        }
        Ok(())
    }

    /// Store root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The app's directory in the store (`apps/<pkg>@<signer>`): its
    /// `data/` subtree is the runtime data dir.
    pub fn app_dir(&self, app: &AppId) -> PathBuf {
        self.root.join("apps").join(app.dir_name())
    }
}

fn render_record(r: &InstallRecord) -> String {
    // Simple key=value record; values are already validated/hex or decimal.
    format!(
        "package={}\nsigner={}\nversion_code={}\napk_sha256={}\ninstalled_at={}\n",
        r.app.package, r.app.signer, r.version_code, r.apk_sha256, r.installed_at
    )
}

fn parse_record(text: &str, root: &Path) -> Result<InstallRecord> {
    let mut package = None;
    let mut signer = None;
    let mut version_code = None;
    let mut apk_sha256 = None;
    let mut installed_at = 0;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "package" => package = Some(v.to_string()),
            "signer" => signer = Some(v.to_string()),
            "version_code" => version_code = v.parse().ok(),
            "apk_sha256" => apk_sha256 = Some(v.to_string()),
            "installed_at" => installed_at = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    let app = AppId {
        package: package.ok_or_else(|| err("install.record.corrupt"))?,
        signer: signer.ok_or_else(|| err("install.record.corrupt"))?,
    };
    let hash = apk_sha256.ok_or_else(|| err("install.record.corrupt"))?;
    Ok(InstallRecord {
        app,
        version_code: version_code.ok_or_else(|| err("install.record.corrupt"))?,
        apk_sha256: hash.clone(),
        apk_path: root.join("apks").join(&hash),
        derived_dir: root.join("derived").join(&hash),
        installed_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppId {
        AppId {
            package: "com.test.app".into(),
            signer: "aa".repeat(32),
        }
    }

    #[test]
    fn install_commit_and_reload() {
        let dir = tempfile_dir();
        let store = InstallStore::open(&dir).unwrap();
        let hash = store.store_apk(b"apk-bytes-v1").unwrap();
        let rec = store
            .commit(&app(), 1, &hash, &"aa".repeat(32))
            .unwrap();
        assert_eq!(rec.version_code, 1);
        let loaded = store.current(&app()).unwrap().unwrap();
        assert_eq!(loaded.apk_sha256, hash);
        assert_eq!(store.apk_bytes(&loaded).unwrap(), b"apk-bytes-v1");
    }

    #[test]
    fn rejects_signer_change() {
        let dir = tempfile_dir();
        let store = InstallStore::open(&dir).unwrap();
        let hash = store.store_apk(b"v1").unwrap();
        store
            .commit(&app(), 1, &hash, &"aa".repeat(32))
            .unwrap();
        // Different app.signer → rejected, and the identity lookup itself is keyed by signer.
        let other = AppId {
            package: "com.test.app".into(),
            signer: "bb".repeat(32),
        };
        assert!(store.commit(&other, 2, &hash, &"bb".repeat(32)).is_ok()); // different identity is a separate app
        // Same identity dir but mismatched caller arg → reject
        let e = store
            .commit(&app(), 2, &hash, &"bb".repeat(32))
            .unwrap_err();
        assert_eq!(e.category(), ErrorCategory::PkgSignature);
        // Old record still active.
        assert_eq!(store.current(&app()).unwrap().unwrap().version_code, 1);
    }

    #[test]
    fn dedups_apk_storage() {
        let dir = tempfile_dir();
        let store = InstallStore::open(&dir).unwrap();
        let h1 = store.store_apk(b"same").unwrap();
        let h2 = store.store_apk(b"same").unwrap();
        assert_eq!(h1, h2);
    }

    fn tempfile_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!("ovelo-apk-test-{}-{}", std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
