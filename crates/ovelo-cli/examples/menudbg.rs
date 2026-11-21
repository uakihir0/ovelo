//! Find NavigationView elements in every layout of an APK and print
//! their attributes; then dump the menu document a `menu` attr
//! references (if any).

use ovelo_dexrt::res::ApkResources;

fn main() {
    let path = std::env::args().nth(1).expect("apk path");
    let bytes = std::fs::read(&path).expect("read apk");
    let Some(r) = ApkResources::open(bytes) else {
        println!("open failed");
        return;
    };
    for id in 0x7f000000u32..0x80000000 {
        let Some(els) = r.layout_elements(id) else {
            continue;
        };
        for e in &els {
            if !e.name.contains("NavigationView") {
                continue;
            }
            println!("layout {id:#x} d{} {}", e.depth, e.name);
            for a in &e.attrs {
                println!("   {:?}={:?}", a.name, a.value);
            }
            for a in &e.attrs {
                if a.name == "menu" {
                    if let ovelo_apk::manifest::Value::Reference(mid) = a.value {
                        println!("menu res {mid:#x}:");
                        match r.document(mid) {
                            Some(doc) => {
                                for el in &doc {
                                    let mut line = format!("  d{} {}", el.depth, el.name);
                                    for at in &el.attrs {
                                        line.push_str(&format!(" {:?}={:?}", at.name, at.value));
                                    }
                                    println!("{line}");
                                }
                            }
                            None => println!("  no document"),
                        }
                    }
                }
            }
        }
    }
}
