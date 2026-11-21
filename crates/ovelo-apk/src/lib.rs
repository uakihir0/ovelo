//! ovelo-apk — APK inspection, signature verification and install records.
//!
//! Implements design doc §03 (APK Installation and Updates): ZIP central
//! directory validation, binary `AndroidManifest.xml` / `resources.arsc`
//! parsing, APK Signature Scheme v2 verification, and the content-hash
//! install store with same-signer updates.

pub mod arsc;
pub mod install;
pub mod manifest;
pub mod sig;
pub mod splits;
pub mod zip;

pub use arsc::{parse_arsc, ResourcePackage};
pub use install::{AppId, InstallRecord, InstallStore, StoredSplit};
pub use manifest::{parse_manifest, Element, ManifestInfo, Value};
pub use sig::{has_v2_signature, has_v3_signature, verify_v2, SignerInfo};
pub use splits::{
    install_split_set, resolve_splits, split_kind, validate_split_set, DeviceProfile, SplitInput,
    SplitKind, SplitMember,
};
pub use zip::{parse_central_directory, read_entry, CentralEntry, ZipLimits};

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

/// Summary of an APK produced by `inspect_apk` (design doc §03 step 4's
/// "compatibility check" input).
#[derive(Debug, Clone)]
pub struct ApkInfo {
    /// Parsed manifest fields.
    pub manifest: ManifestInfo,
    /// Entry names, in archive order.
    pub entries: Vec<String>,
    /// `classes*.dex` entries, in name order (fixed class lookup order).
    pub dex_entries: Vec<String>,
    /// ABIs under `lib/` (e.g. `arm64-v8a`).
    pub abis: Vec<String>,
    /// Whether `resources.arsc` is present.
    pub has_resources: bool,
    /// Whether the APK carries a v2/v3 signature block.
    pub signed_v2_or_v3: bool,
    /// SHA-256 of the whole APK.
    pub sha256: String,
}

/// Inspect APK bytes without installing (§03 steps 1–4).
pub fn inspect_apk(bytes: &[u8]) -> Result<ApkInfo> {
    let (_cd, entries) = parse_central_directory(bytes, &ZipLimits::default())?;
    let mut manifest = None;
    for e in &entries {
        if e.name == "AndroidManifest.xml" {
            let raw = read_entry(bytes, e)?;
            manifest = Some(parse_manifest(&raw)?);
        }
    }
    let manifest = manifest.ok_or_else(|| {
        OveloError::new(
            ErrorCategory::PkgInvalid,
            Phase::Install,
            "apk.manifest.absent",
        )
    })?;

    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    let mut dex_entries: Vec<String> = names
        .iter()
        .filter(|n| n.starts_with("classes") && n.ends_with(".dex"))
        .cloned()
        .collect();
    // Fixed class lookup order: classes.dex, classes2.dex, ...
    dex_entries.sort();

    let abis: std::collections::BTreeSet<String> = names
        .iter()
        .filter_map(|n| {
            let mut it = n.split('/');
            if it.next() == Some("lib") {
                it.next().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();

    Ok(ApkInfo {
        manifest,
        entries: names.clone(),
        dex_entries,
        abis: abis.into_iter().collect(),
        has_resources: names.iter().any(|n| n == "resources.arsc"),
        signed_v2_or_v3: has_v2_signature(bytes) || has_v3_signature(bytes),
        sha256: {
            use sha2::Digest;
            sha2::Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        },
    })
}

/// Verify signature, then commit into `store` (§03 steps 3–5).
///
/// Requires a v2 signature block; updates to the same package must carry the
/// same signer fingerprint.
pub fn install_apk(store: &InstallStore, bytes: &[u8]) -> Result<InstallRecord> {
    let signers = verify_v2(bytes)?;
    // Canonical fingerprint covers *every* signer: an update signed by a
    // strict subset of the original signer set must not pass `commit`.
    let mut hashes: Vec<&str> = signers.iter().map(|s| s.cert_sha256.as_str()).collect();
    hashes.sort_unstable();
    let signer = &hashes.join("+");
    let info = inspect_apk(bytes)?;
    let app = AppId {
        package: info.manifest.package.clone(),
        signer: signer.clone(),
    };
    let apk_hash = store.store_apk(bytes)?;
    store.commit(&app, info.manifest.version_code, &apk_hash, signer)
}
