//! Debug tool: dump method protos / fields / lists from an APK.
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let cls = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(dex) = DexFile::parse(buf) else { continue };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if owner != cls { continue }
            let Ok(data) = dex.class_data(cd) else { println!("no class_data"); continue };
            println!("== {owner}");
            for m in &data.direct_methods {
                let mm = dex.method(m.method_idx).unwrap();
                println!("  D {} code_off={:#x}", dex.string(mm.name_idx).unwrap_or_default(), m.code_off);
            }
            for m in &data.virtual_methods {
                let mm = dex.method(m.method_idx).unwrap();
                println!("  V {} code_off={:#x}", dex.string(mm.name_idx).unwrap_or_default(), m.code_off);
            }
        }
    }
}
