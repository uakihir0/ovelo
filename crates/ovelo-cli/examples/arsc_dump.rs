//! dump arsc entries: key -> res_id -> path
use ovelo_apk::{arsc::parse_arsc, zip::{parse_central_directory, read_entry, ZipLimits}};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap_or_default();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    let e = ents.iter().find(|e| e.name == "resources.arsc").unwrap();
    let arsc = read_entry(&bytes, e).unwrap();
    for pkg in parse_arsc(&arsc).unwrap() {
        for t in &pkg.types {
            for en in &t.entries {
                if !en.key.contains(&pat) { continue; }
                println!("{}/{} {:#x} cfg={} val={:?}", t.name, en.key, en.res_id, en.config,
                    en.value.as_ref().and_then(|v| v.string.clone()).or(en.value.as_ref().map(|v| format!("d{:#x}", v.data))).unwrap_or_default());
            }
        }
    }
}
