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

/// Executables a distribution carries: the file itself, or every
/// regular file under `Contents/MacOS` for an `.app`-style bundle
/// (symlinks to shared libraries are not executables). Bundled helpers
/// get the same signature/entitlement audit — a helper with looser
/// permissions must not slip the gate (§10).
fn binaries(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let macos = path.join("Contents").join("MacOS");
    let mut bins: Vec<std::path::PathBuf> = if macos.is_dir() {
        // file_type() does not follow symlinks: a `Contents/MacOS`
        // entry pointing at a shared library is not an executable and
        // cannot carry hardened-runtime flags — skip it.
        std::fs::read_dir(&macos)
            .map_err(|e| io_err("read_dir", e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect()
    } else {
        Vec::new()
    };
    bins.sort();
    if bins.is_empty() {
        return Err(OveloError::new(
            ErrorCategory::PkgInvalid,
            Phase::Install,
            "gate.bundle.macos",
        )
        .with_detail(path.display().to_string()));
    }
    Ok(bins)
}

fn io_err(op: &str, e: std::io::Error) -> OveloError {
    OveloError::new(ErrorCategory::HostCrash, Phase::Service, format!("gate.{op}"))
        .with_detail(e.to_string())
}

/// Merge per-binary verdicts: any Fail wins, else Warn, else Skip,
/// else Pass.
fn combine(name: &'static str, items: Vec<GateItem>) -> GateItem {
    for v in ["fail", "warn", "skip"] {
        if let Some(i) = items.iter().find(|i| {
            matches!(
                (&i.verdict, v),
                (Verdict::Fail(_), "fail")
                    | (Verdict::Warn(_), "warn")
                    | (Verdict::Skip(_), "skip")
            )
        }) {
            return GateItem {
                name,
                verdict: i.verdict.clone(),
            };
        }
    }
    GateItem {
        name,
        verdict: Verdict::Pass,
    }
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
    let bins = binaries(path)?;
    Ok(vec![
        combine("signature", bins.iter().map(|b| check_signature(b)).collect()),
        combine(
            "entitlements",
            bins.iter().map(|b| check_entitlements(b)).collect(),
        ),
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
                // spctl passing on a locally-trusted signature says
                // nothing about notarization — only the Notarized
                // Developer ID source counts for distribution.
                GateItem {
                    name: "notarization",
                    verdict: Verdict::Fail("signed but not notarized".into()),
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
/// runtime builds — on a `CodeDirectory` line, so match the flag list
/// itself: an `Executable=` path containing "runtime" is not a flag.
fn hardened_runtime(dv_output: &str) -> bool {
    dv_output.lines().any(|line| {
        let Some(pos) = line.find("flags=") else {
            return false;
        };
        let rest = line[pos + "flags=".len()..]
            .trim_start_matches(|c: char| c.is_ascii_hexdigit() || c == 'x');
        let Some(inner) = rest.strip_prefix('(').and_then(|r| r.split(')').next()) else {
            return false;
        };
        inner.split(',').any(|f| f.trim() == "runtime")
    })
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
        assert!(hardened_runtime("flags=0x10002(kill,runtime)"));
        assert!(hardened_runtime(
            "CodeDirectory v=20500 size=123 flags=0x10002(kill,runtime) hashes=4"
        ));
        assert!(!hardened_runtime("flags=0x0(none)"));
        assert!(!hardened_runtime(""));
        assert!(!hardened_runtime(
            "Executable=/tmp/runtime-app/Foo\nCodeDirectory v=20500 flags=0x0(none)"
        ));
    }

    #[test]
    fn extracts_entitlement_keys() {
        let plist = "<dict>\n  <key>com.apple.security.cs.allow-jit</key>\n  <true/>\n</dict>";
        assert_eq!(entitlement_keys(plist), ["com.apple.security.cs.allow-jit"]);
    }
}
