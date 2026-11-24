//! dump full signatures (name+proto desc) of methods declared on classes
//! whose descriptor matches a substring: msig <apk> <owner_pat>
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let Ok(dex) = DexFile::parse(read_entry(&bytes, e).unwrap()) else {
            continue;
        };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains(&pat) {
                continue;
            }
            let Ok(data) = dex.class_data(cd) else {
                continue;
            };
            for m in data
                .direct_methods
                .iter()
                .chain(data.virtual_methods.iter())
            {
                let Ok(mm) = dex.method(m.method_idx) else {
                    continue;
                };
                let n = dex.string(mm.name_idx).unwrap_or_default();
                let sig = (|| {
                    let p = dex.proto(mm.proto_idx.into()).ok()?;
                    let r = dex.type_descriptor(p.return_type_idx).ok()?;
                    let ps: Vec<String> = dex
                        .type_list(p.parameters_off)
                        .ok()?
                        .iter()
                        .map(|&i| {
                            dex.type_descriptor(i.into())
                                .unwrap_or_default()
                                .to_string()
                        })
                        .collect();
                    Some(format!("({}){}", ps.join(""), r))
                })()
                .unwrap_or_else(|| "(?)".to_string());
                println!("{owner}.{n}{sig}");
            }
        }
    }
}
