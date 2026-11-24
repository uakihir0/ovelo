//! find methods whose code references a given string constant
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let needle = std::env::args().nth(2).unwrap();
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
        // string table index of the needle
        let mut want = Vec::new();
        for i in 0..dex.strings_len() as u32 {
            if dex.string(i).unwrap_or_default().contains(&needle) {
                want.push(i);
            }
        }
        if want.is_empty() {
            continue;
        }
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            let Ok(data) = dex.class_data(cd) else {
                continue;
            };
            for m in data
                .direct_methods
                .iter()
                .chain(data.virtual_methods.iter())
            {
                let Ok(Some(code)) = dex.code_item(m.code_off) else {
                    continue;
                };
                let Ok(insns) = decode_all(&code.insns) else {
                    continue;
                };
                let hit = insns.iter().any(|ins| {
                    ins.name.starts_with("const-string")
                        && ins.idx.is_some_and(|ix| want.contains(&ix))
                });
                if hit {
                    let Ok(mm) = dex.method(m.method_idx) else {
                        continue;
                    };
                    let mn = dex.string(mm.name_idx).unwrap_or_default();
                    println!("== {owner}.{mn} midx={}", m.method_idx);
                }
            }
        }
    }
}
