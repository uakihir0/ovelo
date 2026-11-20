//! Print manifest application class + startup initializers.
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let b = std::fs::read(&p).unwrap();
    let m = ovelo_apk::inspect_apk(&b).unwrap().manifest;
    println!("app class: {:?}", m.application_class);
    println!("initializers: {:?}", m.startup_initializers);
}
