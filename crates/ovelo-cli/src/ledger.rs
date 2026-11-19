//! Compatibility ledger (design §14): every evaluate/run against an APK
//! appends a JSON record to `<store>/ledger/<package>.jsonl`. PASS is
//! granted only to the (APK sha, scenario, version) combination. Records
//! never contain content typed into the app, credentials, or personal
//! file names — only APK- and environment-derived fields.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use std::path::{Path, PathBuf};

/// One row of the compatibility ledger.
#[derive(Debug, Clone)]
pub struct LedgerRecord {
    /// Artifact sha256 — the fixture identity.
    pub fixture_sha256: String,
    /// Package name.
    pub package: String,
    /// versionCode.
    pub version_code: i64,
    /// targetSdk when declared.
    pub target_sdk: Option<i64>,
    /// Canonical signer fingerprint.
    pub signer: String,
    /// OS + arch of the evaluating host.
    pub os: String,
    /// Execution backend used ("interpreter", "aot", "inspect-only").
    pub backend: String,
    /// Scenario name, e.g. `diagnose`, `install`, `launch`.
    pub scenario: String,
    /// Stable result category: `pass` or an error category/cause.
    pub result: String,
    /// Extra detail (category causes, dex names — never user content).
    pub detail: String,
    /// Runtime version that produced the record.
    pub runtime_version: String,
}

impl LedgerRecord {
    /// Serialize to one JSON line (escapes kept minimal — fields are
    /// structured values, not free text).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"fixture_sha256\":{:?},\"package\":{:?},\"version_code\":{},\"target_sdk\":{},\"signer\":{:?},\"os\":{:?},\"backend\":{:?},\"scenario\":{:?},\"result\":{:?},\"detail\":{:?},\"runtime_version\":{:?}}}",
            self.fixture_sha256,
            self.package,
            self.version_code,
            self.target_sdk
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
            self.signer,
            self.os,
            self.backend,
            self.scenario,
            self.result,
            self.detail,
            self.runtime_version,
        )
    }
}

/// Append-only ledger store under the install store root.
pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    /// Ledger for a store root (`<root>/ledger/`).
    pub fn at(store_root: &Path) -> Self {
        Self {
            dir: store_root.join("ledger"),
        }
    }

    /// Append `rec` to the package's `.jsonl`; returns the file path.
    pub fn record(&self, rec: &LedgerRecord) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            OveloError::new(ErrorCategory::HostCrash, Phase::Service, "ledger.mkdir")
                .with_detail(e.to_string())
        })?;
        let file = self.dir.join(format!("{}.jsonl", sanitize(&rec.package)));
        let mut line = rec.to_json();
        line.push('\n');
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .map_err(|e| {
                OveloError::new(ErrorCategory::HostCrash, Phase::Service, "ledger.write")
                    .with_detail(e.to_string())
            })?;
        f.write_all(line.as_bytes()).map_err(|e| {
            OveloError::new(ErrorCategory::HostCrash, Phase::Service, "ledger.write")
                .with_detail(e.to_string())
        })?;
        Ok(file)
    }
}

/// Package names can't carry path separators into the ledger filename.
fn sanitize(pkg: &str) -> String {
    pkg.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Host OS string for the record (`macos-aarch64` style).
pub fn host_os() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> LedgerRecord {
        LedgerRecord {
            fixture_sha256: "ab".repeat(32),
            package: "com.a".into(),
            version_code: 3,
            target_sdk: Some(35),
            signer: "cd".repeat(32),
            os: host_os(),
            backend: "inspect-only".into(),
            scenario: "diagnose".into(),
            result: "pass".into(),
            detail: String::new(),
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    #[test]
    fn appends_json_lines() {
        let dir = std::env::temp_dir().join(format!("ovelo-ledger-{}", std::process::id()));
        let l = Ledger::at(&dir);
        let f = l.record(&rec()).unwrap();
        l.record(&rec()).unwrap();
        let body = std::fs::read_to_string(f).unwrap();
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("\"scenario\":\"diagnose\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_sdk_null_when_absent() {
        let mut r = rec();
        r.target_sdk = None;
        assert!(r.to_json().contains("\"target_sdk\":null"));
    }
}
