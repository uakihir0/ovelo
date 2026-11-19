//! `.app` bundle layout (design doc §08 "Launch, switch, quit").
//!
//! Ovelo apps launch as real macOS `.app` bundles: LaunchServices sees a
//! normal package with an `Info.plist`, a `Contents/MacOS` executable stub
//! and `Contents/Resources` (icon + installed APK reference). This module is
//! structural: it computes the bundle's file layout — writing to disk and
//! `lsregister` are the installer CLI's job.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::HostCrash, Phase::Host, cause)
}

/// Everything needed to materialize one app bundle.
#[derive(Debug, Clone)]
pub struct LaunchRecord {
    /// macOS bundle name (`Foo.app`).
    pub app_name: String,
    /// `CFBundleIdentifier` — reverse-DNS, derived from the Android package.
    pub bundle_id: String,
    /// Android package name (e.g. `com.example.app`).
    pub package: String,
    /// Path to the installed APK inside the container (written verbatim into
    /// `Info.plist` as `OveloApkPath`).
    pub apk_path: String,
    /// App version string (`CFBundleShortVersionString`).
    pub version: String,
}

impl LaunchRecord {
    /// Validation shared by every consumer.
    pub fn validate(&self) -> Result<()> {
        if self.app_name.is_empty() || self.app_name.contains('/') {
            return Err(err("host.launch.name"));
        }
        if self.bundle_id.is_empty() || self.bundle_id.contains(' ') {
            return Err(err("host.launch.bundle_id"));
        }
        if self.apk_path.is_empty() {
            return Err(err("host.launch.apk"));
        }
        Ok(())
    }
}

/// `Info.plist` XML for the record.
fn info_plist(r: &LaunchRecord) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n<dict>\n",
            "\t<key>CFBundleName</key>\n\t<string>{name}</string>\n",
            "\t<key>CFBundleIdentifier</key>\n\t<string>{bid}</string>\n",
            "\t<key>CFBundleExecutable</key>\n\t<string>{name}</string>\n",
            "\t<key>CFBundlePackageType</key>\n\t<string>APPL</string>\n",
            "\t<key>CFBundleShortVersionString</key>\n\t<string>{ver}</string>\n",
            "\t<key>LSMinimumSystemVersion</key>\n\t<string>14.0</string>\n",
            "\t<key>NSHighResolutionCapable</key>\n\t<true/>\n",
            "\t<key>LSUIElement</key>\n\t<false/>\n",
            "\t<key>OveloApkPackage</key>\n\t<string>{pkg}</string>\n",
            "\t<key>OveloApkPath</key>\n\t<string>{apk}</string>\n",
            "</dict>\n</plist>\n"
        ),
        name = esc(&r.app_name),
        bid = esc(&r.bundle_id),
        ver = esc(&r.version),
        pkg = esc(&r.package),
        apk = esc(&r.apk_path),
    )
}

/// One file of the bundle: path relative to `Foo.app/` and its contents.
/// Executables/scripts are returned with `executable = true`.
#[derive(Debug)]
pub struct BundleFile {
    /// Bundle-relative path (e.g. `Contents/Info.plist`).
    pub path: String,
    /// File contents.
    pub contents: Vec<u8>,
    /// Whether the file needs the executable bit.
    pub executable: bool,
}

/// Shell single-quote: `'` inside single quotes cannot appear, so close the
/// quote, emit an escaped `'`, and reopen (`'\''`). This blocks `$()`,
/// backticks, word splitting and globbing — only `'` needs handling.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Launcher stub: execs `ovelo run <apk>` so the bundle works from Finder,
/// Spotlight and `open` without extra wrapping.
fn launcher_stub(r: &LaunchRecord) -> String {
    format!(
        "#!/bin/sh\nexec ovelo run --apk {} --package {}\n",
        sh_quote(&r.apk_path),
        sh_quote(&r.package),
    )
}

/// Full `.app` bundle file layout for a launch record. Returns files in a
/// deterministic order (deterministic = diffable in the compat ledger).
pub fn bundle_layout(record: &LaunchRecord) -> Result<Vec<BundleFile>> {
    record.validate()?;
    Ok(vec![
        BundleFile {
            path: "Contents/Info.plist".into(),
            contents: info_plist(record).into_bytes(),
            executable: false,
        },
        BundleFile {
            path: format!("Contents/MacOS/{}", record.app_name),
            contents: launcher_stub(record).into_bytes(),
            executable: true,
        },
        // Contents/Resources/AppIcon.icns is written by the installer when an
        // icon is extracted from the APK; absent → Finder default icon.
        BundleFile {
            path: "Contents/Resources/.ovelo-package".into(),
            contents: record.package.clone().into_bytes(),
            executable: false,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> LaunchRecord {
        LaunchRecord {
            app_name: "Demo".into(),
            bundle_id: "ai.ovelo.demo".into(),
            package: "com.example.demo".into(),
            apk_path: "/var/ovelo/pkg/demo/base.apk".into(),
            version: "1.0".into(),
        }
    }

    #[test]
    fn layout_is_complete_and_deterministic() {
        let a = bundle_layout(&rec()).unwrap();
        let b = bundle_layout(&rec()).unwrap();
        let pa: Vec<_> = a.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            pa,
            [
                "Contents/Info.plist",
                "Contents/MacOS/Demo",
                "Contents/Resources/.ovelo-package"
            ]
        );
        assert!(a[1].executable);
        let plist = String::from_utf8(a[0].contents.clone()).unwrap();
        assert!(plist.contains("<string>ai.ovelo.demo</string>"));
        assert!(plist.contains("OveloApkPath"));
        assert_eq!(a[0].contents, b[0].contents);
    }

    #[test]
    fn rejects_bad_records() {
        for r in [
            LaunchRecord {
                app_name: "a/b".into(),
                ..rec()
            },
            LaunchRecord {
                bundle_id: "has space".into(),
                ..rec()
            },
            LaunchRecord {
                apk_path: String::new(),
                ..rec()
            },
        ] {
            assert!(bundle_layout(&r).is_err());
        }
    }
}
