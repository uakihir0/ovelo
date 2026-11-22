//! Debug tool: dump method protos / fields / lists from an APK.
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let bytes = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue; }
        let Ok(dex) = DexFile::parse(read_entry(&bytes, e).unwrap()) else { continue };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains("internal/connection/RealCall;") { continue; }
            let Ok(data) = dex.class_data(cd) else { continue };
            for m in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
                let Ok(mm) = dex.method(m.method_idx) else { continue };
                let n = dex.string(mm.name_idx).unwrap_or_default();
                if n.contains("InterceptorChain") || n.contains("execute") || n.contains("enqueue") {
                    let p = dex.proto(mm.proto_idx as u32).unwrap();
                let params: Vec<String> = dex.proto_params(p).unwrap_or_default().iter().map(|t| dex.type_descriptor(*t as u32).unwrap_or_default()).collect();
                let ret = dex.type_descriptor(p.return_type_idx).unwrap_or_default();
                let proto = format!("({}){}", params.join(""), ret);
                    println!("{owner}.{n} -> {proto}");
                }
            }
        }
    }
}
