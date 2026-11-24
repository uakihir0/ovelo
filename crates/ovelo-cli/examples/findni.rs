//! find methods containing a type-indexed op on a given type descriptor
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let op_pat = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "new-instance".into());
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
                    ins.name == op_pat
                        && ins
                            .idx
                            .and_then(|ix| dex.type_descriptor(ix).ok())
                            .as_deref()
                            == Some(pat.as_str())
                });
                if hit {
                    let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
                    let mm = dex.method(m.method_idx).unwrap();
                    let mn = dex.string(mm.name_idx).unwrap_or_default();
                    println!("{owner};.{mn}");
                }
            }
        }
    }
}
