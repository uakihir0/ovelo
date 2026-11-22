//! Debug helper: dump manifest providers + startup initializers.
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&p).unwrap();
    let m = ovelo_apk::inspect_apk(&bytes).unwrap().manifest;
    for pr in &m.providers {
        println!("provider: {pr}");
    }
    for i in &m.startup_initializers {
        println!("init: {i}");
    }
}
