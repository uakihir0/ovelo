//! Dump field_ids whose owner matches a substring.
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
        for fidx in 0..d.field_ids_len() {
            let Ok(f) = d.field(fidx as u32) else {
                continue;
            };
            let Ok(owner) = d.type_descriptor(f.class_idx as u32) else {
                continue;
            };
            if !owner.contains(&owner_pat) {
                continue;
            }
            let name = d.string(f.name_idx).unwrap_or_default();
            let ty = d.type_descriptor(f.type_idx as u32).unwrap_or_default();
            println!("{owner}->{name}:{ty}");
        }
    }
}
