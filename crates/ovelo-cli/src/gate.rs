//! Distribution gate (design P4): before a build is published, its
//! signature, hardened runtime, entitlements, and notarization are checked
//! with `codesign`/`spctl`. Executable-memory exceptions may only be the
//! allowlisted JIT/unsigned-memory keys the runtime needs.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use std::path::Path;
use std::process::Command;

/// Entitlement keys the runtime is allowed to carry (§10: attach
/// executable-memory exceptions only to binaries that need them).
const ALLOWED_ENTITLEMENTS: &[&str] = &[
    "com.apple.security.cs.allow-jit",
    "com.apple.security.cs.allow-unsigned-executable-memory",
];

/// One gate item and its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Requirement met.
    Pass,
    /// Requirement not met — blocks distribution.
    Fail(String),
    /// Met, but worth flagging (e.g. unsigned-executable-memory present).
    Warn(String),
    /// Could not be evaluated on this host (e.g. no codesign).
    Skip(String),
}

/// A named check result.
#[derive(Debug)]
pub struct GateItem {
    /// Check name, e.g. `signature`.
    pub name: &'static str,
    /// Verdict.
    pub verdict: Verdict,
}

/// Run every gate check against the binary/app at `path`.
pub fn evaluate(path: &Path) -> Result<Vec<GateItem>> {
    if !path.exists() {
        return Err(OveloError::new(
            ErrorCategory::PkgInvalid,
            Phase::Install,
            "gate.path.missing",
        )
        .with_detail(path.display().to_string()));
    }
    Ok(vec![
        check_signature(path),
        check_entitlements(path),
        check_notarization(path),
    ])
}

/// True when every item is Pass or Warn (Fail/Skip block).
pub fn passed(items: &[GateItem]) -> bool {
    items
        .iter()
        .all(|i| matches!(i.verdict, Verdict::Pass | Verdict::Warn(_)))
}

fn run(cmd: &str, args: &[&str]) -> Result<(String, String)> {
    let out = Command::new(cmd).args(args).output().map_err(|e| {
        OveloError::new(ErrorCategory::HostCrash, Phase::Service, "gate.tool.missing")
            .with_detail(format!("{cmd}: {e}"))
    })?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn check_signature(path: &Path) -> GateItem {
    let v = Command::new("codesign")
        .args(["--verify", "--deep", "--strict", "--verbose=2"])
        .arg(path)
        .output();
    match v {
        Err(e) => GateItem {
            name: "signature",
            verdict: Verdict::Skip(format!("codesign: {e}")),
        },
        Ok(o) if o.status.success() => {
            let (_out, err) = (
                String::from_utf8_lossy(&o.stdout).into_owned(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            );
            // `codesign -dv` prints flags=0x10000(runtime) when hardened.
            let dv = Command::new("codesign")
                .args(["-dv", "--verbose=2"])
                .arg(path)
                .output();
            let flags = dv
                .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                .unwrap_or_default();
            if hardened_runtime(&flags) {
                GateItem {
                    name: "signature",
                    verdict: Verdict::Pass,
                }
            } else {
                GateItem {
                    name: "signature",
                    verdict: Verdict::Fail(format!("signed but no hardened runtime: {}", err.trim())),
                }
            }
        }
        Ok(o) => GateItem {
            name: "signature",
            verdict: Verdict::Fail(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        },
    }
}

fn check_entitlements(path: &Path) -> GateItem {
    match run("codesign", &["-d", "--entitlements", ":-"]) {
        Err(e) => GateItem {
            name: "entitlements",
            verdict: Verdict::Skip(e.to_string()),
        },
        Ok(_) => {
            let out = Command::new("codesign")
                .args(["-d", "--entitlements", ":-"])
                .arg(path)
                .output();
            match out {
                Err(e) => GateItem {
                    name: "entitlements",
                    verdict: Verdict::Skip(format!("codesign: {e}")),
                },
                Ok(o) if !o.status.success() => GateItem {
                    name: "entitlements",
                    verdict: Verdict::Fail(
                        String::from_utf8_lossy(&o.stderr).trim().to_string(),
                    ),
                },
                Ok(o) => {
                    let body = String::from_utf8_lossy(&o.stdout);
                    let keys = entitlement_keys(&body);
                    let bad: Vec<&String> = keys
                        .iter()
                        .filter(|k| !ALLOWED_ENTITLEMENTS.contains(&k.as_str()))
                        .collect();
                    if !bad.is_empty() {
                        GateItem {
                            name: "entitlements",
                            verdict: Verdict::Fail(format!(
                                "unlisted entitlements: {}",
                                bad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                            )),
                        }
                    } else if keys
                        .iter()
                        .any(|k| k == "com.apple.security.cs.allow-unsigned-executable-memory")
                    {
                        GateItem {
                            name: "entitlements",
                            verdict: Verdict::Warn(
                                "unsigned executable memory allowed".into(),
                            ),
                        }
                    } else {
                        GateItem {
                            name: "entitlements",
                            verdict: Verdict::Pass,
                        }
                    }
                }
            }
        }
    }
}

fn check_notarization(path: &Path) -> GateItem {
    let out = Command::new("spctl")
        .args(["-a", "-t", "exec", "-vv"])
        .arg(path)
        .output();
    match out {
        Err(e) => GateItem {
            name: "notarization",
            verdict: Verdict::Skip(format!("spctl: {e}")),
        },
        Ok(o) => {
            let body = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if body.contains("source=Notarized Developer ID") {
                GateItem {
                    name: "notarization",
                    verdict: Verdict::Pass,
                }
            } else if o.status.success() {
                GateItem {
                    name: "notarization",
                    verdict: Verdict::Warn("signed but not notarized".into()),
                }
            } else {
                GateItem {
                    name: "notarization",
                    verdict: Verdict::Fail(body.trim().to_string()),
                }
            }
        }
    }
}

/// `codesign -dv` stderr carries `flags=0x10000(runtime)` for hardened
/// runtime builds.
fn hardened_runtime(flags_line: &str) -> bool {
    flags_line.contains("(runtime)")
}

/// Pull entitlement keys out of a `codesign --entitlements` plist body.
fn entitlement_keys(body: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("<key>") {
            if let Some(k) = rest.strip_suffix("</key>") {
                keys.push(k.to_string());
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hardened_runtime_flag() {
        assert!(hardened_runtime("flags=0x10000(runtime)"));
        assert!(!hardened_runtime("flags=0x0(none)"));
        assert!(!hardened_runtime(""));
    }

    #[test]
    fn extracts_entitlement_keys() {
        let plist = "<dict>\n  <key>com.apple.security.cs.allow-jit</key>\n  <true/>\n</dict>";
        assert_eq!(entitlement_keys(plist), ["com.apple.security.cs.allow-jit"]);
    }
}
