//! Dump each `<init>`'s parameter descriptors for classes matching a substring.
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let owner_pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue; }
        let buf = read_entry(&bytes, e).unwrap();
        let dex = match DexFile::parse(buf) { Ok(d) => d, Err(_) => continue };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains(&owner_pat) { continue; }
            let Ok(data) = dex.class_data(cd) else { continue };
            for m in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
                let Ok(mm) = dex.method(m.method_idx) else { continue };
                let mn = dex.string(mm.name_idx).unwrap_or_default();
                if mn != "<init>" { continue; }
                let Ok(proto) = dex.proto(mm.proto_idx.into()) else { continue };
                let params: Vec<String> = match dex.proto_params(proto) {
                    Ok(list) => list.iter().map(|t| dex.type_descriptor((*t).into()).unwrap_or_default()).collect(),
                    Err(_) => vec![],
                };
                println!("{owner}.<init>({})", params.join(","));
            }
        }
    }
}
