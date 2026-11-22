//! Dump method_ids whose owner matches a substring, with proto sigs.
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let owner_pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(d) = DexFile::parse(buf) else { continue };
        for midx in 0..d.method_ids_len() {
            let Ok(m) = d.method(midx as u32) else { continue };
            let Ok(owner) = d.type_descriptor(m.class_idx as u32) else {
                continue;
            };
            if !owner.contains(&owner_pat) {
                continue;
            }
            let name = d.string(m.name_idx).unwrap_or_default();
            let Ok(proto) = d.proto(m.proto_idx as u32) else { continue };
            let ret = d
                .type_descriptor(proto.return_type_idx as u32)
                .unwrap_or_default();
            let mut sig = String::from("(");
            if let Ok(params) = d.proto_params(proto) {
                for t in params {
                    sig.push_str(&d.type_descriptor(t as u32).unwrap_or_default());
                }
            }
            sig.push(')');
            sig.push_str(&ret);
            println!("{owner}->{name}{sig}");
        }
    }
}
