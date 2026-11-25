//! find layout xmls containing given tag names
use ovelo_apk::{
    manifest::parse_axml,
    zip::{parse_central_directory, read_entry, ZipLimits},
};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat: Vec<String> = std::env::args().skip(2).collect();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("res/") || !e.name.ends_with(".xml") {
            continue;
        }
        let Ok(xml) = read_entry(&bytes, e) else {
            continue;
        };
        let Ok(els) = parse_axml(&xml) else { continue };
        let names: Vec<String> = els.iter().map(|el| el.name.clone()).collect();
        if pat
            .iter()
            .all(|p| names.iter().any(|n| n.contains(p.as_str())))
        {
            println!("{}: {:?}", e.name, names);
        }
    }
}
