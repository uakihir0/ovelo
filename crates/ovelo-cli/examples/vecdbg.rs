//! Dump `<vector>` drawable docs found by entry name — e.g.
//! `vecdbg app.apk ic_plus_vector` prints the AXML attributes.

use ovelo_dexrt::res::ApkResources;

fn main() {
    let path = std::env::args().nth(1).expect("apk path");
    let want: Vec<String> = std::env::args().skip(2).collect();
    let bytes = std::fs::read(&path).expect("read apk");
    let Some(r) = ApkResources::open(bytes) else {
        println!("open failed");
        return;
    };
    let mut vectors = 0;
    let mut drawables = 0;
    for id in 0x7f000000u32..0x80000000 {
        if r.type_name(id) != Some("drawable") {
            continue;
        }
        drawables += 1;
        let name = r.entry_name(id).unwrap_or("?").to_string();
        let hit = want.is_empty() || want.iter().any(|w| name.contains(w.as_str()));
        let Some(doc) = r.document(id) else { continue };
        let is_vec = doc.iter().any(|e| e.name == "vector");
        if is_vec {
            vectors += 1;
        }
        if hit {
            println!("{id:#x} {name} vec={is_vec}");
            for e in &doc {
                let mut line = format!("  d{} {}", e.depth, e.name);
                for a in &e.attrs {
                    line.push_str(&format!(" {:?}={:?}", a.name, a.value));
                }
                println!("{line}");
            }
        }
    }
    println!("drawables={drawables} vectors={vectors}");
}
