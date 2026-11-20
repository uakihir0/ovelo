//! Probe dex class definitions (name search + type_index_of sanity checks).
use ovelo_dex::DexFile;
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
fn main() {
    let apk = std::fs::read("/Users/devin/apk-samples/newpipe_1017.apk").unwrap();
    let (_, entries) = parse_central_directory(&apk, &ZipLimits::default()).unwrap();
    for e in &entries {
        if !e.name.ends_with(".dex") { continue; }
        let bytes = read_entry(&apk, e).unwrap();
        let d = DexFile::parse(bytes).unwrap();
        let mut hits = 0;
        for def in d.class_defs() {
            let s = d.type_descriptor(def.class_idx).unwrap_or_default();
            if s.contains("NavigationView") || s.contains("FocusAware") {
                println!("{} class_idx={} = {}", e.name, def.class_idx, s);
                hits += 1;
            }
        }
        // direct type_index_of probes
        for q in [
            "Lcom/google/android/material/navigation/NavigationView;",
            "Lorg/schabi/newpipe/views/FocusAwareDrawerLayout;",
            "Landroid/widget/LinearLayout;",
        ] {
            println!("{} type_index_of({q}) = {:?}", e.name, d.type_index_of(q));
        }
        println!("{}: {} class-def hits of {} defs", e.name, hits, d.class_defs().len());
    }
}
