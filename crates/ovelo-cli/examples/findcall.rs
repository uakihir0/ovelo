//! find callers of a named method in an apk's dexes
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let target = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue; }
        let buf = read_entry(&bytes, e).unwrap();
        let dex = match DexFile::parse(buf) { Ok(d) => d, Err(_) => continue };
        let mut tgt = std::collections::HashSet::new();
        for mid in 0..dex.method_ids_len() {
            let m = dex.method(mid as u32).unwrap();
            if dex.string(m.name_idx).unwrap_or_default() == target { tgt.insert(mid as u32); }
        }
        if tgt.is_empty() { continue; }
        for cd in dex.class_defs() {
            let Ok(data) = dex.class_data(cd) else { continue };
            for m in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
                let Ok(Some(code)) = dex.code_item(m.code_off) else { continue };
                let Ok(insns) = decode_all(&code.insns) else { continue };
                for ins in &insns {
                    if let Some(ix) = ins.idx {
                        if tgt.contains(&ix) {
                            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_else(|_| "?".into());
                            let mn = dex.string(dex.method(m.method_idx).unwrap().name_idx).unwrap_or_else(|_| "?".into());
                            println!("{} {owner}.{mn} -> {target}", e.name);
                        }
                    }
                }
            }
        }
    }
}
