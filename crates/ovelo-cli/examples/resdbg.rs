//! Dump the parsed resource index for an APK: layout count, a sample of
//! layout ids/paths, id-name resolution, and whether a given layout
//! res_id parses to elements.

use ovelo_dexrt::res::ApkResources;

fn main() {
    let path = std::env::args().nth(1).expect("apk path");
    let bytes = std::fs::read(&path).expect("read apk");
    let Some(r) = ApkResources::open(bytes) else {
        println!("open failed");
        return;
    };
    // Probe well-known layout range by walking entry_names.
    let mut layouts = 0usize;
    for id in 0x7f000000u32..0x80000000 {
        if let Some(p) = r.layout_path(id) {
            layouts += 1;
            if layouts <= 15 {
                println!("layout {id:#x} {p}");
            }
        }
    }
    println!("total layouts: {layouts}");
    for name in ["activity_main", "main"] {
        if let Some(id) = r.id_named(name) {
            println!("id {name} = {id:#x}");
        }
    }
    // Optional: dump one specific layout id in full.
    if let Some(hex) = std::env::args().nth(2) {
        let id = u32::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
        println!("layout {id:#x} path={:?}", r.layout_path(id));
        match r.layout_elements(id) {
            Some(els) => {
                println!("layout {id:#x}: {} elements", els.len());
                for e in &els {
                    let mut line = format!("  d{} {}", e.depth, e.name);
                    for a in &e.attrs {
                        line.push_str(&format!(" {:?}={:?}", a.name, a.value));
                    }
                    println!("{line}");
                }
            }
            None => println!("layout {id:#x}: no elements"),
        }
        return;
    }
    // Parse the first layout found to validate axml.
    for id in 0x7f000000u32..0x80000000 {
        if r.layout_path(id).is_some() {
            match r.layout_elements(id) {
                Some(els) => {
                    println!("layout {id:#x}: {} elements", els.len());
                    for e in els.iter().take(8) {
                        println!("  d{} {}", e.depth, e.name);
                    }
                }
                None => println!("layout {id:#x}: axml parse failed"),
            }
            break;
        }
    }
}
