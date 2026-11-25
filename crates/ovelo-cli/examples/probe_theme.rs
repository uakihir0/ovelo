//! probe_theme - dump a style res-id items and theme lookups (debug).
use ovelo_dexrt::res::ApkResources;
fn main() {
    let path = std::env::args().nth(1).expect("apk");
    let bytes = std::fs::read(&path).unwrap();
    let info = ovelo_apk::inspect_apk(&bytes).expect("inspect");
    println!("launcher: {:?}", info.manifest.launcher_activity);
    println!(
        "app theme: {:?}",
        info.manifest.theme.map(|t| format!("{t:#x}"))
    );
    for (k, v) in &info.manifest.activity_themes {
        println!("activity_theme {k} -> {v:#x}");
    }
    let r = ApkResources::open(bytes).unwrap();
    let style: u32 = std::env::args()
        .nth(2)
        .map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap())
        .unwrap_or(0x7f11014b);
    let items = r.style_attrs(style);
    println!("style {style:#x} items={}", items.len());
    for (k, v) in &items {
        println!(
            "   {k:#x} ty{:02x} {:#x} {:?}",
            v.data_type, v.data, v.string
        );
    }
    for attr in [
        0x1010056u32,
        0x10102cd,
        0x7f0403f7,
        0x7f040400,
        0x7f0403f8,
        0x7f0403f9,
        0x1010057,
    ] {
        println!(
            "   style_item {attr:#x} -> {:?}",
            r.style_item(style, attr).map(|v| (v.data_type, v.data))
        );
    }
    let mut cur = style;
    let mut chain = vec![];
    for _ in 0..8 {
        chain.push(format!("{cur:#x}"));
        match r.style_parent(cur) {
            Some(p) if p != 0 => cur = p,
            _ => break,
        }
    }
    println!("   chain: {}", chain.join(" -> "));
}
