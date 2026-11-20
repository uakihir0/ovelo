//! Debug helper: dump style items / attr resolution for a style res id.
use ovelo_dexrt::res::ApkResources;
fn main() {
    let path = std::env::args().nth(1).expect("apk");
    let bytes = std::fs::read(&path).unwrap();
    let info = ovelo_apk::inspect_apk(&bytes).unwrap();
    let r = ApkResources::open(bytes).unwrap();
    let theme = info.manifest.theme.unwrap_or(0);
    let mut cur = theme;
    for depth in 0..12 {
        let items = r.style_attrs(cur);
        println!("d{depth} style {cur:#x} name={:?} items={}", r.entry_name(cur), items.len());
        for (k, v) in items.iter().take(6) {
            println!("     attr {k:#x} = ty{:02x} {:08x}", v.data_type, v.data);
        }
        match r.style_parent(cur) {
            Some(p) if p != 0 => cur = p,
            _ => break,
        }
    }
}
