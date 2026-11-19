//! APK split sets (design §03): a base APK plus `config.*` splits must
//! share package / versionCode / signer; the device profile picks the
//! matching ABI, density, and locale splits at install time. A split set
//! missing its base, or a `configForSplit` reference to an absent split,
//! is rejected before anything is committed.

use crate::install::{InstallRecord, InstallStore};
use crate::manifest::ManifestInfo;
use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

/// Host-side capabilities a config split is matched against.
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    /// Preferred ABI names, most preferred first.
    pub abis: Vec<String>,
    /// Screen density bucket (dpi) to match `config.*dpi` splits.
    pub density: u32,
    /// Preferred locale tags, most preferred first.
    pub locales: Vec<String>,
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            abis: vec!["arm64-v8a".into(), "armeabi-v7a".into()],
            density: 420,
            locales: vec!["en".into()],
        }
    }
}

/// What one config split carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitKind {
    /// The base APK (no `split` attribute).
    Base,
    /// `config.<abi>` native-library split.
    Abi(String),
    /// `config.<N>dpi` resource split.
    Density(u32),
    /// `config.<locale>` resource split.
    Locale(String),
    /// Any other named split (feature splits included).
    Feature(String),
}

/// ABI names as they appear in `config.*` split names (`config.x86`,
/// `config.arm64-v8a`). `_` is accepted as a separator variant.
const KNOWN_ABIS: &[&str] = &[
    "armeabi",
    "armeabi-v7a",
    "arm64-v8a",
    "x86",
    "x86-64",
    "riscv64",
    // Fat split carrying every ABI — acts as the fallback candidate.
    "universal",
];

const DENSITY_BUCKETS: &[(&str, u32)] = &[
    ("ldpi", 120),
    ("mdpi", 160),
    ("tvdpi", 213),
    ("hdpi", 240),
    ("xhdpi", 320),
    ("xxhdpi", 480),
    ("xxxhdpi", 640),
];

/// Classify a manifest `split` attribute; `None` (absent attr) is `Base`.
pub fn split_kind(split: Option<&str>) -> SplitKind {
    let Some(name) = split else {
        return SplitKind::Base;
    };
    let Some(cfg) = name.strip_prefix("config.") else {
        return SplitKind::Feature(name.to_string());
    };
    if let Some(d) = DENSITY_BUCKETS.iter().find(|(n, _)| *n == cfg) {
        return SplitKind::Density(d.1);
    }
    // Numeric densities ('420dpi') need no bucket entry.
    if let Some(dpi) = cfg
        .strip_suffix("dpi")
        .and_then(|n| n.parse::<u32>().ok())
    {
        return SplitKind::Density(dpi);
    }
    // `_` is a separator variant (`config.arm64_v8a`).
    let norm = cfg.replace('_', "-");
    if KNOWN_ABIS.contains(&norm.as_str()) {
        return SplitKind::Abi(norm);
    }
    // Locale qualifiers are `lang` or `lang-rREGION`.
    if let Some((lang, region)) = norm.split_once("-r") {
        if !lang.is_empty()
            && !region.is_empty()
            && lang.chars().all(|c| c.is_ascii_alphabetic())
            && region.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return SplitKind::Locale(cfg.to_string());
        }
    }
    // Underscore/hyphen locales without the `r` prefix (`en_US`, `en-US`)
    // still read as language tags: an all-alpha first segment wins.
    if let Some(first) = norm.split('-').next() {
        if norm.contains('-') && first.chars().all(|c| c.is_ascii_alphabetic()) {
            return SplitKind::Locale(cfg.to_string());
        }
    }
    // Remaining hyphenated names default to ABI; bare words are locales.
    if norm.contains('-') {
        return SplitKind::Abi(norm);
    }
    SplitKind::Locale(cfg.to_string())
}

/// One validated member of a split set.
#[derive(Debug, Clone)]
pub struct SplitMember {
    /// `split` attribute value (`""` for the base APK).
    pub name: String,
    /// Classified kind.
    pub kind: SplitKind,
    /// Stored content hash of this split APK.
    pub sha256: String,
    /// `configForSplit` target, when declared.
    pub config_for: Option<String>,
}

/// A split APK before validation: its manifest + signer fingerprint +
/// content hash (computed by the caller from the APK bytes).
#[derive(Debug)]
pub struct SplitInput {
    /// File name for error detail.
    pub file_name: String,
    /// Parsed manifest.
    pub manifest: ManifestInfo,
    /// Canonical signer fingerprint (see `install_apk`).
    pub signer: String,
    /// Stored sha256 (already passed through `store_apk`).
    pub sha256: String,
}

/// Validate a whole split set: one base, identical package/versionCode/
/// signer, unique names, and every `configForSplit` resolved. Returns
/// the members in input order on success.
pub fn validate_split_set(inputs: &[SplitInput]) -> Result<Vec<SplitMember>> {
    if inputs.is_empty() {
        return Err(err("split.set.empty"));
    }
    let base_manifest = &inputs[0].manifest;
    let mut seen = std::collections::HashSet::new();
    let mut members = Vec::with_capacity(inputs.len());
    let mut bases = 0usize;
    for i in inputs {
        let m = &i.manifest;
        if m.package != base_manifest.package {
            return Err(err("split.package.mismatch").with_detail(format!(
                "{}: {} != {}",
                i.file_name, m.package, base_manifest.package
            )));
        }
        if m.version_code != base_manifest.version_code {
            return Err(err("split.version.mismatch").with_detail(format!(
                "{}: {} != {}",
                i.file_name, m.version_code, base_manifest.version_code
            )));
        }
        if i.signer != inputs[0].signer {
            return Err(OveloError::new(
                ErrorCategory::PkgSignature,
                Phase::Install,
                "split.signer.mismatch",
            )
            .with_detail(i.file_name.clone()));
        }
        let kind = split_kind(m.split.as_deref());
        if kind == SplitKind::Base {
            bases += 1;
        }
        let name = m.split.clone().unwrap_or_default();
        if !seen.insert(name.clone()) {
            return Err(err("split.duplicate").with_detail(name));
        }
        members.push(SplitMember {
            name,
            kind,
            sha256: i.sha256.clone(),
            config_for: m.config_for_split.clone(),
        });
    }
    if bases != 1 {
        return Err(err("split.base.count").with_detail(format!("{bases}")));
    }
    // configForSplit must point at a member of this set.
    let names: std::collections::HashSet<&str> =
        members.iter().map(|m| m.name.as_str()).collect();
    for m in &members {
        if let Some(t) = &m.config_for {
            if !names.contains(t.as_str()) {
                return Err(err("split.config_target.missing").with_detail(format!(
                    "{} -> {t}",
                    m.name
                )));
            }
        }
    }
    Ok(members)
}

/// Locale tags normalize to `lang-region` lowercase (`en-rUS`, `en_US`,
/// `en-US` all collapse).
fn norm_locale(l: &str) -> String {
    l.replace('_', "-").replace("-r", "-").to_lowercase()
}

/// Pick the members to install for `profile`: the base always; for each
/// config dimension the best-matching split (first in `abis`/`locales`
/// order; nearest density bucket); feature splits pass through.
pub fn resolve_splits<'m>(members: &'m [SplitMember], profile: &DeviceProfile) -> Vec<&'m SplitMember> {
    let mut picked: Vec<&SplitMember> = Vec::new();
    let mut abi: Option<&SplitMember> = None;
    let mut density: Option<&SplitMember> = None;
    let mut locales: Vec<&SplitMember> = Vec::new();
    for m in members {
        match &m.kind {
            SplitKind::Base | SplitKind::Feature(_) => picked.push(m),
            SplitKind::Abi(_) => {
                // `universal` ranks after every listed ABI but still
                // serves as the fallback when nothing specific matches.
                let rank = |s: &SplitMember| {
                    let SplitKind::Abi(n) = &s.kind else { return usize::MAX };
                    profile
                        .abis
                        .iter()
                        .position(|p| p == n)
                        .unwrap_or(if n == "universal" { profile.abis.len() } else { usize::MAX })
                };
                let better = match abi {
                    None => rank(m) != usize::MAX,
                    Some(cur) => rank(m) < rank(cur),
                };
                if better {
                    abi = Some(m);
                }
            }
            SplitKind::Density(d) => {
                let better = match density {
                    None => true,
                    Some(cur) => {
                        let SplitKind::Density(cd) = cur.kind else { unreachable!() };
                        (d.abs_diff(profile.density), m.name.as_str())
                            < (cd.abs_diff(profile.density), cur.name.as_str())
                    }
                };
                if better {
                    density = Some(m);
                }
            }
            SplitKind::Locale(l) => {
                let want = norm_locale(l);
                if profile
                    .locales
                    .iter()
                    .map(|p| norm_locale(p))
                    .any(|p| p == want || p.starts_with(&format!("{want}-")))
                {
                    locales.push(m);
                }
            }
        }
    }
    picked.extend(abi);
    picked.extend(density);
    picked.extend(locales);
    picked
}

/// Install a split set: validate it, resolve for `profile`, then commit
/// the base record listing the stored member hashes. `inputs` are
/// `(file_name, bytes)` pairs; every split must carry a v2 signature.
pub fn install_split_set(
    store: &InstallStore,
    inputs: &[(&str, &[u8])],
    profile: &DeviceProfile,
) -> Result<InstallRecord> {
    let mut members_in = Vec::with_capacity(inputs.len());
    for (name, bytes) in inputs {
        let signers = crate::sig::verify_v2(bytes)?;
        let mut hashes: Vec<&str> = signers.iter().map(|s| s.cert_sha256.as_str()).collect();
        hashes.sort_unstable();
        let signer = hashes.join("+");
        let info = crate::inspect_apk(bytes)?;
        let sha = store.store_apk(bytes)?;
        members_in.push(SplitInput {
            file_name: (*name).to_string(),
            manifest: info.manifest,
            signer,
            sha256: sha,
        });
    }
    let members = validate_split_set(&members_in)?;
    let picked = resolve_splits(&members, profile);
    // Commit happens against the base member's record.
    let base = members
        .iter()
        .find(|m| m.kind == SplitKind::Base)
        .ok_or_else(|| err("split.base.count"))?;
    let picked_splits: Vec<crate::install::StoredSplit> = picked
        .iter()
        .map(|m| crate::install::StoredSplit {
            name: m.name.clone(),
            sha256: m.sha256.clone(),
        })
        .collect();
    let app = crate::install::AppId {
        package: members_in[0].manifest.package.clone(),
        signer: members_in[0].signer.clone(),
    };
    store.commit_splits(
        &app,
        members_in[0].manifest.version_code,
        &base.sha256,
        &members_in[0].signer,
        &picked_splits,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(name: &str, split: Option<&str>, pkg: &str, vc: i64, signer: &str) -> SplitInput {
        SplitInput {
            file_name: name.to_string(),
            manifest: ManifestInfo {
                package: pkg.into(),
                version_code: vc,
                split: split.map(|s| s.to_string()),
                ..Default::default()
            },
            signer: signer.into(),
            sha256: "ab".repeat(32),
        }
    }

    #[test]
    fn classifies_split_names() {
        assert_eq!(split_kind(None), SplitKind::Base);
        assert_eq!(split_kind(Some("config.arm64-v8a")), SplitKind::Abi("arm64-v8a".into()));
        assert_eq!(split_kind(Some("config.xhdpi")), SplitKind::Density(320));
        assert_eq!(split_kind(Some("config.ja")), SplitKind::Locale("ja".into()));
        assert_eq!(split_kind(Some("config.x86")), SplitKind::Abi("x86".into()));
        assert_eq!(split_kind(Some("config.x86_64")), SplitKind::Abi("x86-64".into()));
        assert_eq!(split_kind(Some("config.arm64_v8a")), SplitKind::Abi("arm64-v8a".into()));
        assert_eq!(split_kind(Some("config.420dpi")), SplitKind::Density(420));
        assert_eq!(split_kind(Some("config.en-rUS")), SplitKind::Locale("en-rUS".into()));
        assert_eq!(split_kind(Some("config.en_US")), SplitKind::Locale("en_US".into()));
        assert_eq!(split_kind(Some("feature_x")), SplitKind::Feature("feature_x".into()));
    }

    #[test]
    fn accepts_valid_set() {
        let set = vec![
            inp("base.apk", None, "com.a", 1, "s"),
            inp("abi.apk", Some("config.arm64-v8a"), "com.a", 1, "s"),
            inp("en.apk", Some("config.en"), "com.a", 1, "s"),
        ];
        assert_eq!(validate_split_set(&set).unwrap().len(), 3);
    }

    #[test]
    fn rejects_missing_base_and_mismatched_fields() {
        assert!(validate_split_set(&[inp("a.apk", Some("config.en"), "com.a", 1, "s")]).is_err());
        assert!(validate_split_set(&[
            inp("b.apk", None, "com.a", 1, "s"),
            inp("a.apk", Some("config.en"), "com.b", 1, "s"),
        ]).is_err());
        assert!(validate_split_set(&[
            inp("b.apk", None, "com.a", 1, "s"),
            inp("a.apk", Some("config.en"), "com.a", 2, "s"),
        ]).is_err());
        assert!(validate_split_set(&[
            inp("b.apk", None, "com.a", 1, "s"),
            inp("a.apk", Some("config.en"), "com.a", 1, "other"),
        ]).is_err());
        assert!(validate_split_set(&[
            inp("b1.apk", None, "com.a", 1, "s"),
            inp("b2.apk", None, "com.a", 1, "s"),
        ]).is_err());
    }

    #[test]
    fn resolves_best_config_for_profile() {
        let set = vec![
            inp("base.apk", None, "com.a", 1, "s"),
            inp("arm.apk", Some("config.arm64-v8a"), "com.a", 1, "s"),
            inp("x86.apk", Some("config.x86_64-linux"), "com.a", 1, "s"),
            inp("hd.apk", Some("config.xhdpi"), "com.a", 1, "s"),
            inp("en.apk", Some("config.en"), "com.a", 1, "s"),
            inp("ja.apk", Some("config.ja"), "com.a", 1, "s"),
        ];
        let members = validate_split_set(&set).unwrap();
        let picked = resolve_splits(&members, &DeviceProfile::default());
        let names: Vec<&str> = picked.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&""));
        assert!(names.contains(&"config.arm64-v8a"));
        assert!(names.contains(&"config.xhdpi"));
        assert!(names.contains(&"config.en"));
        assert!(!names.contains(&"config.x86_64-linux"));
        assert!(!names.contains(&"config.ja"));
    }

    #[test]
    fn region_profile_matches_language_split() {
        let set = vec![
            inp("base.apk", None, "com.a", 1, "s"),
            inp("en.apk", Some("config.en"), "com.a", 1, "s"),
            inp("gb.apk", Some("config.en-rGB"), "com.a", 1, "s"),
        ];
        let members = validate_split_set(&set).unwrap();
        let profile = DeviceProfile {
            locales: vec!["en-US".into()],
            ..Default::default()
        };
        let names: Vec<&str> = resolve_splits(&members, &profile)
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(names.contains(&"config.en"));
        assert!(!names.contains(&"config.en-rGB"));
    }

    #[test]
    fn universal_abi_is_the_fallback_candidate() {
        let set = vec![
            inp("base.apk", None, "com.a", 1, "s"),
            inp("uni.apk", Some("config.universal"), "com.a", 1, "s"),
        ];
        let members = validate_split_set(&set).unwrap();
        // Explicitly listed.
        let p = DeviceProfile {
            abis: vec!["universal".into()],
            ..Default::default()
        };
        let names: Vec<&str> = resolve_splits(&members, &p)
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(names.contains(&"config.universal"));
        // Also picked when no specific ABI matches.
        let p2 = DeviceProfile {
            abis: vec!["mips".into()],
            ..Default::default()
        };
        let names: Vec<&str> = resolve_splits(&members, &p2)
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(names.contains(&"config.universal"));
    }
}
