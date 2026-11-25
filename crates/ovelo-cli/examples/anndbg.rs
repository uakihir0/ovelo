//! Check AnnotationDefault elements parse on annotation classes.
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::{DexFile, EncodedValue};
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
        let mut dirs = 0usize;
        let mut meths = 0usize;
        let mut items_n = 0usize;
        let mut anncls = 0usize;
        for def in d.class_defs() {
            if def.access_flags & 0x2000 != 0 {
                anncls += 1;
            }
            let owner = d.type_descriptor(def.class_idx).unwrap_or_default();
            if !owner.contains(&owner_pat) {
                continue;
            }
            let is_ann = def.access_flags & 0x2000 != 0;
            let dir = match d.annotations_directory(def) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("dir err {owner}: {e}");
                    continue;
                }
            };
            if is_ann {
                println!(
                    "ANNCLS {owner} ann_off={:#x} class_off={:#x} methods={} params={}",
                    def.annotations_off,
                    dir.class_annotations_off,
                    dir.method_annotations.len(),
                    dir.param_annotations.len()
                );
            }
            // scan class + field + param sets for AnnotationDefault
            let scan = |kind: &str, off: u32| {
                if let Ok(items) = d.annotation_set(off) {
                    for it in &items {
                        let ty = d.type_descriptor(it.type_idx).unwrap_or_default();
                        if ty.contains("AnnotationDefault") {
                            let names: Vec<String> = it
                                .elements
                                .iter()
                                .map(|(ni, _)| d.string(*ni).unwrap_or_default())
                                .collect();
                            println!("FOUND {kind} {owner} @{off:#x} @{ty} elems={:?}", names);
                        }
                    }
                }
            };
            if dir.class_annotations_off != 0 {
                scan("class", dir.class_annotations_off);
            }
            for (fi, off) in &dir.field_annotations {
                let _ = fi;
                scan("field", *off);
            }
            for (mi, off) in &dir.param_annotations {
                let _ = mi;
                scan("param", *off);
            }
            dirs += 1;
            meths += dir.method_annotations.len();
            for (mi, off) in &dir.method_annotations {
                let items = match d.annotation_set(*off) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("set err {owner} m#{mi} @{off:#x}: {e}");
                        continue;
                    }
                };
                items_n += items.len();
                for it in &items {
                    let ty = d.type_descriptor(it.type_idx).unwrap_or_default();
                    if def.access_flags & 0x2000 != 0 {
                        println!("ANNCLASS {owner} m#{} @{ty}", mi);
                    }
                    if ty.contains("annotation") || ty.contains("Annotation") {
                        println!("SYS {owner} @{ty}");
                    }
                    if !ty.contains("AnnotationDefault") {
                        continue;
                    }
                    let mn = d
                        .method(*mi)
                        .and_then(|m| d.string(m.name_idx))
                        .unwrap_or_default();
                    for (ni, ev) in &it.elements {
                        let en = d.string(*ni).unwrap_or_default();
                        let txt = match ev {
                            EncodedValue::Type(t) => d.type_descriptor(*t).unwrap_or_default(),
                            EncodedValue::Enum(fi) | EncodedValue::Field(fi) => d
                                .field(*fi)
                                .and_then(|f| d.string(f.name_idx))
                                .unwrap_or_default(),
                            EncodedValue::String(s) => {
                                format!("{:?}", d.string(*s).unwrap_or_default())
                            }
                            EncodedValue::Int(v) => format!("{v}"),
                            EncodedValue::Long(v) => format!("{v}"),
                            other => format!("{other:?}"),
                        };
                        println!("{owner}.{mn} @{ty} {en}={txt}");
                    }
                }
            }
        }
        println!(
            "{}: {} dirs, {} method_anns, {} items, {} ann-classes",
            e.name, dirs, meths, items_n, anncls
        );
    }
}
