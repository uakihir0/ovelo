//! Dump class-level annotation types for classes matching a pattern.
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(d) = DexFile::parse(buf) else { continue };
        for def in d.class_defs() {
            let owner = d.type_descriptor(def.class_idx).unwrap_or_default();
            if !owner.contains(&pat) {
                continue;
            }
            let Ok(dir) = d.annotations_directory(def) else {
                continue;
            };
            if dir.class_annotations_off == 0 {
                println!("{owner}: NO class annotation set");
                continue;
            }
            match d.annotation_set(dir.class_annotations_off) {
                Ok(items) => {
                    let tys: Vec<String> = items
                        .iter()
                        .map(|it| {
                            let ty = d.type_descriptor(it.type_idx).unwrap_or_default();
                            let els: Vec<String> = it
                                .elements
                                .iter()
                                .map(|(ni, _)| d.string(*ni).unwrap_or_default())
                                .collect();
                            format!("{ty}{els:?}")
                        })
                        .collect();
                    println!("{owner}: {} anns -> {tys:?}", items.len());
                }
                Err(e) => println!("{owner}: set err {e}"),
            }
        }
    }
}
