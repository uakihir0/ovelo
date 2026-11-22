//! Debug tool: dump method protos / fields / lists from an APK.
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") { continue; }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(d) = DexFile::parse(buf) else { continue };
        for cd in d.class_defs() {
            let owner = d.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains(&pat) { continue; }
            println!("{owner} class_data_off={:#x}", cd.class_data_off);
            match d.class_data_rc(cd) {
                Ok(c) => {
                    println!("  instance_fields={}", c.instance_fields.len());
                    for f in &c.instance_fields {
                        if let Ok(fi) = d.field(f.field_idx) {
                            let n = d.string(fi.name_idx).unwrap_or_else(|e| format!("<err {e:?}>"));
                            let t = d.type_descriptor(fi.type_idx as u32).unwrap_or_default();
                            println!("    {}:{}", n, t);
                        } else {
                            println!("    field[{}] <unresolved>", f.field_idx);
                        }
                    }
                }
                Err(e) => println!("  class_data err {e:?}"),
            }
        }
    }
}
