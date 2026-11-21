//! Debug helper example (resource/layout dump).

use ovelo_dexrt::res::ApkResources;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let r = ApkResources::open(bytes).unwrap();
    for mid in [0x7f1600f5u32, 0x7f1600f3, 2131623941, 2131623939] {
        let Some(doc) = r.document(mid) else {
            println!("{mid:#x}: none");
            continue;
        };
        println!("menu {mid:#x}:");
        for el in &doc {
            let mut l = format!("  d{} {}", el.depth, el.name);
            for a in &el.attrs {
                l.push_str(&format!(" {}={:?}", a.name, a.value));
            }
            println!("{l}");
        }
    }
}
