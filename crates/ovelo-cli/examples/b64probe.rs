//! Probe kotlin.io.encoding.Base64.decode$default on a known string.
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
use ovelo_dexrt::{Vm, V};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let input = std::env::args().nth(2).unwrap_or_else(|| "QUJD".into());
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
    let dflt = vm.get_static_named("Lkotlin/io/encoding/Base64;", "Default");
    println!("Default = {dflt:?}");
    let sh = vm.intern_str(&input);
    let r = vm.run(
        "Lkotlin/io/encoding/Base64;",
        "decode$default",
        &[dflt.unwrap_or(V::Ref(0)), V::Ref(sh)],
    );
    println!("decode({input:?}) -> {r:?}");
}
