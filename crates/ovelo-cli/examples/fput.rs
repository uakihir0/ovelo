use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let owner = std::env::args().nth(2).unwrap();
    let fname = std::env::args().nth(3).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue; }
        let buf = read_entry(&bytes, e).unwrap();
        let dex = match DexFile::parse(buf) { Ok(d) => d, Err(_) => continue };
        let mut tgt = std::collections::HashSet::new();
        for fid in 0..dex.field_ids_len() {
            let f = dex.field(fid as u32).unwrap();
            let fo = dex.type_descriptor(f.class_idx.into()).unwrap_or_default();
            let fn_ = dex.string(f.name_idx).unwrap_or_default();
            if fo.contains(&owner) && fn_.contains(&fname) {
                let ty = dex.type_descriptor(f.type_idx.into()).unwrap_or_default();
                println!("{} field {fo}.{fn_} : {ty}", e.name);
                tgt.insert(fid as u32);
            }
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
                            let o = dex.type_descriptor(cd.class_idx).unwrap_or_default();
                            let mn = dex.string(dex.method(m.method_idx).unwrap().name_idx).unwrap_or_default();
                            println!("  used in {o}.{mn} ({})", ins.name);
                        }
                    }
                }
            }
        }
    }
}
