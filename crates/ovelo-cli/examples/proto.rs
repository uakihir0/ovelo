//! Debug tool: dump method protos / fields / lists from an APK.
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(dex) = DexFile::parse(buf) else { continue };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains("ResponseBody") { continue }
            let Ok(data) = dex.class_data(cd) else { continue };
            for m in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
                let mm = dex.method(m.method_idx).unwrap();
                let mn = dex.string(mm.name_idx).unwrap_or_default();
                if !mn.contains(&pat) { continue }
                let p = dex.proto(mm.proto_idx as u32).unwrap();
                let ret = dex.type_descriptor(p.return_type_idx).unwrap_or_default();
                let params: Vec<String> = dex.type_list(p.parameters_off).unwrap_or_default()
                    .iter().filter_map(|t| dex.type_descriptor(*t as u32).ok()).collect();
                println!("{owner}.{mn}({}){ret}", params.join(""));
            }
        }
    }
}
