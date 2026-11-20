//! dump elements of one res/ xml file by path
use ovelo_apk::{manifest::{parse_axml, Value}, zip::{parse_central_directory, read_entry, ZipLimits}};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let path = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    let e = ents.iter().find(|e| e.name == path).unwrap();
    let xml = read_entry(&bytes, e).unwrap();
    for el in parse_axml(&xml).unwrap() {
        let mut attrs = String::new();
        for a in &el.attrs {
            let v = match &a.value {
                Value::Reference(r) => format!("@{r:#x}"),
                Value::Str(s) => s.clone(),
                Value::Int(i) => format!("{i}"),
                Value::Bool(b) => format!("{b}"),
                Value::Float(f) => format!("{f}"),
                Value::Null => "null".to_string(),
                Value::Other { data_type, data } => format!("t{data_type:#x}:{data:#x}"),
            };
            attrs.push_str(&format!(" {}={}", a.name, v));
        }
        println!("d{} {}{}", el.depth, el.name, attrs);
    }
}
