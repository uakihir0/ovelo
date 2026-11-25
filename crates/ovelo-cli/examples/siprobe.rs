//! Checks whether `SignInPrompt.INSTANCE` resolves inside the tasks dex.

use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
use ovelo_dexrt::Vm;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    let mut vm: Option<Vm> = None;
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(d) = DexFile::parse(buf) else { continue };
        match &mut vm {
            None => vm = Some(Vm::new(d).unwrap()),
            Some(v) => v.add_dex(d).unwrap(),
        }
    }
    let mut vm = vm.unwrap();
    let v = vm.get_static_named("Lorg/tasks/filters/SignInPrompt;", "INSTANCE");
    println!("INSTANCE = {v:?}");
}
