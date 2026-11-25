//! find methods that write (iput/sput) a given field Owner.Name
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let owner_pat = std::env::args().nth(2).unwrap(); // e.g. Li5;
    let name_pat = std::env::args().nth(3).unwrap(); // e.g. C
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
        let mut want: Vec<u32> = Vec::new();
        for i in 0..dex.field_ids_len() as u32 {
            if let Ok(f) = dex.field(i) {
                let fo = dex.type_descriptor(f.class_idx.into()).unwrap_or_default();
                let fn_ = dex.string(f.name_idx).unwrap_or_default();
                if fo == owner_pat && fn_ == name_pat {
                    want.push(i);
                }
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
                if insns.iter().any(|ins| {
                    (ins.name.starts_with("iput") || ins.name.starts_with("sput"))
                        && ins.idx.is_some_and(|ix| want.contains(&ix))
                }) {
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
