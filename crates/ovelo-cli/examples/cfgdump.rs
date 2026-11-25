//! Dev probe: dump a parsed `resources.arsc` / entry config for one APK.

use ovelo_apk::{
    arsc::parse_arsc,
    zip::{parse_central_directory, read_entry, ZipLimits},
};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    let e = ents.iter().find(|e| e.name == "resources.arsc").unwrap();
    let arsc = read_entry(&bytes, e).unwrap();
    for pkg in parse_arsc(&arsc).unwrap() {
        for t in &pkg.types {
            for en in &t.entries {
                if en.res_id == 0x7f060547 || en.key.contains("neutral_text") {
                    println!(
                        "{} {:#x} cfg={:?} val={:?}",
                        en.key,
                        en.res_id,
                        en.config,
                        en.value.as_ref().map(|v| (v.data_type, v.data))
                    );
                }
            }
        }
    }
}
