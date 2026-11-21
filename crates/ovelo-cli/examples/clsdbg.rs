//! dump superclass + interfaces of classes matching a substring
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::DexFile;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let buf = read_entry(&bytes, e).unwrap();
        let dex = match DexFile::parse(buf) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains(&pat) {
                continue;
            }
            let sup = dex
                .type_descriptor(cd.superclass_idx)
                .unwrap_or_else(|_| "?".into());
            println!("{owner} extends {sup}");
            for t in dex.type_list(cd.interfaces_off).unwrap_or_default() {
                println!("  impl {}", dex.type_descriptor(t as u32).unwrap_or_default());
            }
        }
    }
}
