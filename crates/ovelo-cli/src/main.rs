//! ovelo CLI: `inspect` / `install` / `run` / `diagnose` (design doc §12).
//!
//! Exit codes: 0 ok, 1 runtime error (OveloError printed as
//! `CATEGORY [phase] cause`), 2 usage.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ovelo_apk::install::{AppId, InstallRecord, InstallStore};
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_apk::{inspect_apk, install_apk, ApkInfo};
use ovelo_dex::DexFile;
use ovelo_error::{ErrorCategory, OveloError, Phase};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

/// The default install store root: `~/.ovelo/store`.
fn default_store() -> Result<InstallStore, OveloError> {
    let home = std::env::var_os("HOME").ok_or_else(|| err("cli.home.missing"))?;
    InstallStore::open(PathBuf::from(home).join(".ovelo").join("store"))
}

enum Cmd {
    Inspect { apk: PathBuf, json: bool },
    Install { apk: PathBuf, json: bool },
    Run { app: String },
    Diagnose { apk: PathBuf, json: bool },
    Help,
}

fn parse_args(argv: &[String]) -> Result<Cmd, OveloError> {
    let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("help");
    // `--` ends option parsing so `-foo.apk` filenames still work —
    // flags after it are positional args, not options.
    let mut json = false;
    let mut pos: Vec<&String> = Vec::new();
    let mut seen_sep = false;
    let mut unknown_flag = false;
    for a in argv.iter().skip(2) {
        if !seen_sep && a == "--" {
            seen_sep = true;
        } else if !seen_sep && a == "--json" {
            json = true;
        } else if !seen_sep && a.starts_with('-') {
            unknown_flag = true;
        } else {
            pos.push(a);
        }
    }
    if sub != "help" && sub != "--help" && sub != "-h" && (pos.len() != 1 || unknown_flag) {
        return Err(err("cli.usage"));
    }
    let arg = pos.first().copied();
    match (sub, arg) {
        ("inspect", Some(p)) => Ok(Cmd::Inspect {
            apk: p.into(),
            json,
        }),
        ("install", Some(p)) => Ok(Cmd::Install {
            apk: p.into(),
            json,
        }),
        ("run", Some(a)) => Ok(Cmd::Run { app: a.clone() }),
        ("diagnose", Some(p)) => Ok(Cmd::Diagnose {
            apk: p.into(),
            json,
        }),
        ("help", _) | ("--help", _) | ("-h", _) => Ok(Cmd::Help),
        _ => Err(err("cli.usage")),
    }
}

const USAGE: &str = "ovelo — Android app compatibility runtime for macOS

USAGE:
    ovelo inspect  <apk> [--json]   manifest/features/ABIs/dex inventory
    ovelo install  <apk> [--json]   verify signature + commit to the store
    ovelo run      <package>        launch the installed app
    ovelo diagnose <apk>  [--json]  full compatibility report
";

fn read_apk(p: &Path) -> Result<Vec<u8>, OveloError> {
    std::fs::read(p).map_err(|e| {
        err("cli.apk.read").with_detail(format!("{}: {e}", p.display()))
    })
}

/// Escape control characters before printing untrusted APK fields to a
/// terminal — a crafted versionName must not be able to inject escape
/// sequences and alter what the user sees.
fn disp(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// `{:?}`-formatted JSON for an `Option` (`null` or the value).
fn jopt<T: std::fmt::Debug>(o: &Option<T>) -> String {
    match o {
        Some(v) => format!("{v:?}"),
        None => "null".to_string(),
    }
}

fn print_inspect(info: &ApkInfo, json: bool) {
    let m = &info.manifest;
    if json {
        println!("{{");
        println!("  \"package\": {:?},", m.package);
        println!("  \"versionCode\": {},", m.version_code);
        println!("  \"versionName\": {},", jopt(&m.version_name));
        println!("  \"minSdk\": {},", jopt(&m.min_sdk));
        println!("  \"targetSdk\": {},", jopt(&m.target_sdk));
        println!("  \"activities\": {:?},", m.activities);
        println!("  \"features\": {:?},", m.features);
        println!("  \"permissions\": {:?},", m.permissions);
        println!("  \"abis\": {:?},", info.abis);
        println!("  \"dexEntries\": {:?},", info.dex_entries);
        println!("  \"hasResources\": {},", info.has_resources);
        println!("  \"signedV2OrV3\": {},", info.signed_v2_or_v3);
        println!("  \"sha256\": {:?}", info.sha256);
        println!("}}");
    } else {
        println!("package       {}", disp(&m.package));
        println!("version       {} ({})", m.version_code, m.version_name.as_deref().map(disp).unwrap_or_else(|| "-".into()));
        println!("minSdk        {}", m.min_sdk.map(|v| v.to_string()).unwrap_or_else(|| "-".into()));
        println!("targetSdk     {}", m.target_sdk.map(|v| v.to_string()).unwrap_or_else(|| "-".into()));
        println!("activities    {}", disp(&m.activities.join(", ")));
        println!("features      {}", disp(&m.features.join(", ")));
        println!("permissions   {}", disp(&m.permissions.join(", ")));
        println!("abis          {}", info.abis.join(", "));
        println!("dex           {}", info.dex_entries.join(", "));
        println!("resources     {}", if info.has_resources { "yes" } else { "no" });
        println!("signed v2/v3  {}", if info.signed_v2_or_v3 { "yes" } else { "no" });
        println!("sha256        {}", info.sha256);
    }
}

/// Per-dex verification result for the diagnose ledger.
struct DexReport {
    name: String,
    ok: bool,
    cause: String,
}

fn verify_dexes(bytes: &[u8], info: &ApkInfo) -> Result<Vec<DexReport>, OveloError> {
    let (_cd, entries) = parse_central_directory(bytes, &ZipLimits::default())?;
    let mut out = Vec::new();
    for d in &info.dex_entries {
        let e = entries
            .iter()
            .find(|e| &e.name == d)
            .ok_or_else(|| err("cli.dex.missing"))?;
        let raw = read_entry(bytes, e)?;
        let r = DexFile::open(raw).and_then(|f| f.verify());
        out.push(DexReport {
            name: d.clone(),
            ok: r.is_ok(),
            cause: r.err().map(|e| e.cause().to_string()).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Find an installed record for `package` (any signer) in `store`.
fn find_record(store: &InstallStore, package: &str) -> Result<InstallRecord, OveloError> {
    let apps = store.root().join("apps");
    if let Ok(rd) = std::fs::read_dir(&apps) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some((pkg, signer)) = name.split_once('@') {
                if pkg == package {
                    let app = AppId {
                        package: pkg.to_string(),
                        signer: signer.to_string(),
                    };
                    if let Some(rec) = store.current(&app)? {
                        return Ok(rec);
                    }
                }
            }
        }
    }
    Err(err("cli.app.not_installed").with_detail(package))
}

fn run_app(store: &InstallStore, package: &str) -> Result<(), OveloError> {
    let rec = find_record(store, package)?;
    let bytes = store.apk_bytes(&rec)?;
    let info = inspect_apk(&bytes)?;
    // Launch pipeline pieces that exist today: dex parse + verify.
    let dexes = verify_dexes(&bytes, &info)?;
    let bad: Vec<&str> = dexes.iter().filter(|d| !d.ok).map(|d| d.name.as_str()).collect();
    if !bad.is_empty() {
        return Err(OveloError::new(
            ErrorCategory::DexUnsupported,
            Phase::DexLoad,
            "cli.dex.verify",
        )
        .with_detail(bad.join(", ")));
    }
    let launcher = info
        .manifest
        .launcher_activity
        .clone()
        .or_else(|| info.manifest.activities.first().cloned())
        .unwrap_or_default();
    println!("ready   {}", disp(&rec.app.package));
    println!("activity {}", disp(&launcher));
    println!("dexes   {} verified", dexes.len());

    // Launch pipeline: Context → Activity lifecycle (via dexrt when the
    // class is in the DEX) → Presenter → window.
    let data_dir = store.app_dir(&rec.app).join("data");
    let _ctx = ovelo_framework::Context::new(rec.app.package.clone(), data_dir);
    let mut activity = ovelo_framework::Activity::new();
    activity
        .launch()
        .map_err(|e| OveloError::new(ErrorCategory::ServiceMissing, Phase::Service, "cli.run.lifecycle").with_detail(e.to_string()))?;
    let callbacks: Vec<String> = {
        let mut v = Vec::new();
        while let Some(cb) = activity.delivered() {
            v.push(cb.to_string());
        }
        v
    };

    // dexrt dispatch: instantiate the launcher Activity's class, then run
    // each queued lifecycle callback as a virtual call. Callbacks that
    // take a Bundle get a null for now — real marshaling lands with the
    // object bridge; a class missing from the DEX keeps the framework
    // model's behavior.
    let desc = format!("L{};", launcher.replace('.', "/"));
    let desc_safe = disp(&desc);
    let vm = (|| -> Result<_, OveloError> {
        let (_cd, entries) = parse_central_directory(&bytes, &ZipLimits::default())?;
        let e = entries
            .iter()
            .find(|e| e.name == "classes.dex")
            .ok_or_else(|| err("cli.dex.missing"))?;
        let mut v = ovelo_dexrt::Vm::new(DexFile::open(read_entry(&bytes, e)?)?)?;
        v.enable_aot();
        Ok(v)
    })();
    // Guest view bridge: guest view objects (`new TextView` etc.) bind to
    // host widgets hung off this root, and onClick listeners queue back
    // into the VM. Registered BEFORE instantiate so a constructor that
    // builds views in <init>/onCreate hits live stubs.
    let mut fonts = ovelo_ui::FontSet::new();
    fonts.load_dirs(&["/System/Library/Fonts", "/System/Library/Fonts/Supplemental"]);
    let fonts = std::rc::Rc::new(fonts);
    let ui = ovelo_framework::GuestUi::new(720, 1280, fonts);
    let mut guest = vm
        .and_then(|mut v| {
            ovelo_framework::GuestUi::register(&ui, &mut v);
            v.instantiate(&desc).map(|h| (v, h))
        })
        .map_err(|e| {
            println!("guest    {desc_safe} not dispatched: {}", e.cause());
        })
        .ok();
    for cb in callbacks {
        let ran = match &mut guest {
            Some((vm, this)) => {
                let r = vm
                    .call_virtual(*this, &cb, &[])
                    .and_then(|v| match v {
                        s @ Some(_) => Ok(s),
                        None => vm.call_virtual(*this, &cb, &[ovelo_dexrt::V::Ref(0)]),
                    })
                    .inspect_err(|e| println!("callback {cb} crashed: {}", e.cause()));
                matches!(r, Ok(Some(_)))
            }
            None => false,
        };
        println!("callback {} ({})", cb, if ran { "dex" } else { "framework" });
    }
    let mut presenter = ovelo_host::Presenter::new();
    let sid = presenter.attach(3, 720, 1280)?;
    let win = ovelo_host::HostWindow::new(&disp(&rec.app.package), sid, presenter);
    // Fallback demo tree when the Activity wasn't dispatched (e.g. class
    // missing): proves the render+tap path without a guest.
    if guest.is_none() {
        let mut col = ovelo_framework::ViewGroup::new(
            ovelo_framework::Length::MatchParent,
            ovelo_framework::Length::WrapContent,
        );
        let fonts2 = {
            let g = ui.borrow();
            g.fonts.clone()
        };
        let mut title = ovelo_framework::TextView::new(
            rec.app.package.clone(),
            24.0,
            ovelo_ui::Color::srgb(0x20, 0x20, 0x20),
            fonts2.clone(),
        );
        title.padding = 16;
        col.children.push(Box::new(title));
        let taps = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let mut ok = ovelo_framework::Button::new(
            "Tap me",
            18.0,
            ovelo_ui::Color::srgb(0x30, 0x30, 0x30),
            fonts2,
        );
        let taps2 = taps.clone();
        ok.on_click = Some(Box::new(move || {
            taps2.set(taps2.get() + 1);
        }));
        col.children.push(Box::new(ok));
        ui.borrow_mut().root.set_content_view(Box::new(col));
    }
    let ui2 = ui.clone();
    ovelo_host::run(win, move |w| {
        {
            let mut u = ui2.borrow_mut();
            for ev in w.take_input() {
                let Some(pt) = ev.pointers.get(ev.action_index) else {
                    continue;
                };
                match ev.action {
                    ovelo_host::MotionAction::Down => {
                        u.root.dispatch_touch(true, pt.x as i32, pt.y as i32);
                    }
                    ovelo_host::MotionAction::Up => {
                        u.root.dispatch_touch(false, pt.x as i32, pt.y as i32);
                    }
                    _ => {}
                }
            }
        }
        // Deferred onClick dispatches — queued during touch handling,
        // pumped here so the VM is free to run guest code that mutates
        // the same views (RefCell re-entrancy avoided).
        let pending = ui2.borrow().drain_clicks();
        if let Some((vm, _this)) = &mut guest {
            for (l, vh) in pending {
                if let Err(e) = vm.call_virtual(l, "onClick", &[ovelo_dexrt::V::Ref(vh)]) {
                    println!("onClick crashed: {}", e.cause());
                }
            }
        }
        let mut u = ui2.borrow_mut();
        if let Ok(prod) = w.presenter().producer(sid) {
            // Track window resizes: the buffer queue follows the new size
            // and the root re-lays out on the next traversal.
            let (bw, bh) = (prod.queue().width() as i32, prod.queue().height() as i32);
            if (bw, bh) != (u.root.w, u.root.h) {
                u.root.resize(bw, bh);
            }
            let _ = u.root.perform_traversals(prod);
        }
    })
}

fn cmd() -> Result<(), OveloError> {
    let argv: Vec<String> = std::env::args().collect();
    match parse_args(&argv)? {
        Cmd::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Cmd::Inspect { apk, json } => {
            let info = inspect_apk(&read_apk(&apk)?)?;
            print_inspect(&info, json);
            Ok(())
        }
        Cmd::Install { apk, json } => {
            let bytes = read_apk(&apk)?;
            let store = default_store()?;
            let rec = install_apk(&store, &bytes)?;
            if json {
                println!(
                    "{{\n  \"package\": {:?},\n  \"versionCode\": {},\n  \"sha256\": {:?}\n}}",
                    rec.app.package, rec.version_code, rec.apk_sha256
                );
            } else {
                println!(
                    "installed {} v{} -> {}",
                    rec.app.package,
                    rec.version_code,
                    rec.apk_path.display()
                );
            }
            Ok(())
        }
        Cmd::Run { app } => run_app(&default_store()?, &app),
        Cmd::Diagnose { apk, json } => {
            let bytes = read_apk(&apk)?;
            let info = inspect_apk(&bytes)?;
            let dexes = verify_dexes(&bytes, &info)?;
            let sig = if info.signed_v2_or_v3 { "v2/v3" } else { "v1-or-none" };
            if json {
                let dx: Vec<String> = dexes
                    .iter()
                    .map(|d| format!("{{\"name\":{:?},\"ok\":{},\"cause\":{:?}}}", d.name, d.ok, d.cause))
                    .collect();
                println!(
                    "{{\n  \"package\": {:?},\n  \"signature\": {:?},\n  \"abis\": {:?},\n  \"dexes\": [{}],\n  \"sha256\": {:?}\n}}",
                    info.manifest.package,
                    sig,
                    info.abis,
                    dx.join(", "),
                    info.sha256
                );
            } else {
                println!("diagnose {}", disp(&info.manifest.package));
                println!("  signature : {sig}");
                println!("  abis      : {}", info.abis.join(", "));
                for d in &dexes {
                    println!(
                        "  dex {} : {}",
                        d.name,
                        if d.ok { "ok".into() } else { d.cause.clone() }
                    );
                }
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match cmd() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ovelo: {e}");
            if e.cause() == "cli.usage" {
                eprint!("{USAGE}");
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_parse() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_args(&v(&["ovelo", "inspect", "a.apk", "--json"])).unwrap() {
            Cmd::Inspect { apk, json } => {
                assert_eq!(apk, PathBuf::from("a.apk"));
                assert!(json);
            }
            _ => panic!(),
        }
        match parse_args(&v(&["ovelo", "run", "com.x"])).unwrap() {
            Cmd::Run { app } => assert_eq!(app, "com.x"),
            _ => panic!(),
        }
        match parse_args(&v(&["ovelo", "install", "b.apk"])).unwrap() {
            Cmd::Install { json, .. } => assert!(!json),
            _ => panic!(),
        }
        match parse_args(&v(&["ovelo", "diagnose", "d.apk"])).unwrap() {
            Cmd::Diagnose { .. } => {}
            _ => panic!(),
        }
        assert!(parse_args(&v(&["ovelo", "bogus"])).is_err());
        assert!(parse_args(&v(&["ovelo", "inspect"])).is_err());
        assert!(parse_args(&v(&["ovelo", "inspect", "-demo.apk"])).is_err());
        match parse_args(&v(&["ovelo", "inspect", "--", "-demo.apk"])).unwrap() {
            Cmd::Inspect { apk, .. } => assert_eq!(apk, PathBuf::from("-demo.apk")),
            _ => panic!(),
        }
        assert!(parse_args(&v(&["ovelo", "inspect", "a.apk", "extra"])).is_err());
        matches!(parse_args(&v(&["ovelo"])).unwrap(), Cmd::Help);
    }
}
