//! Debug helper example (resource/layout dump).

use ovelo_dexrt::res::ApkResources;
fn main() {
    let path = std::env::args().nth(1).expect("apk path");
    let bytes = std::fs::read(&path).expect("read apk");
    let Some(r) = ApkResources::open(bytes) else { return };
    for id in 0x7f000000u32..0x80000000 {
        let Some(els) = r.layout_elements(id) else { continue };
        let mut hit = false;
        for e in &els {
            if e.name.contains("Toolbar") || e.name.contains("toolbar") {
                if !hit { println!("layout {id:#x}"); hit = true; }
                println!("  {} d{}", e.name, e.depth);
                for a in &e.attrs {
                    println!("     {:?}={:?}", a.name, a.value);
                }
            }
        }
    }
}
