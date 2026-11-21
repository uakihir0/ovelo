//! Debug helper example (resource/layout dump).

use ovelo_dexrt::res::ApkResources;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let id: u32 = u32::from_str_radix(
        std::env::args().nth(2).unwrap().trim_start_matches("0x"),
        16,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let r = ApkResources::open(bytes).unwrap();
    for e in &r.layout_elements(id).unwrap() {
        let mut l = format!("d{} {}", e.depth, e.name);
        for a in &e.attrs {
            l.push_str(&format!(" {}={:?}", a.name, a.value));
        }
        println!("{l}");
    }
}
