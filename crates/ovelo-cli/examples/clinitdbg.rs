//! run a static guest method in a bare Vm and dump the result
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::DexFile;
use ovelo_dexrt::{Obj, HostState, V, Vm};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let class = std::env::args().nth(2).unwrap();
    let method = std::env::args().nth(3).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    let mut dexes: Vec<DexFile> = vec![];
    for e in &ents {
        if e.name.starts_with("classes") && e.name.ends_with(".dex") {
            dexes.push(DexFile::open(read_entry(&bytes, e).unwrap()).unwrap());
        }
    }
    // defining dex first
    let pos = dexes
        .iter()
        .position(|d| {
            d.class_defs().iter().any(|cd| {
                d.type_descriptor(cd.class_idx).ok().as_deref() == Some(class.as_str())
            })
        })
        .unwrap_or(0);
    let mut vm = Vm::new(dexes.swap_remove(pos)).unwrap();
    for d in dexes {
        vm.add_dex(d).unwrap();
    }
    let arg = std::env::args().nth(4);
    let args: Vec<V> = match arg.as_deref() {
        Some(s) => vec![V::Ref(vm.heap.intern_str(s))],
        None => vec![V::Ref(0)],
    };
    match vm.run(&class, &method, &args) {
        Ok(Some(V::Ref(h))) => {
            match vm.heap().get(h) {
                Some(Obj::Host { desc, state, .. }) => {
                    println!("result host {desc} h={h}");
                    match state {
                        HostState::List(l) => {
                            println!("  list len={}", l.len());
                            for (i, v) in l.iter().take(10).enumerate() {
                                let d = if let V::Ref(x) = v {
                                    vm.class_descriptor_of(*x).unwrap_or_default()
                                } else {
                                    format!("{v:?}")
                                };
                                println!("    [{i}] {d}");
                            }
                        }
                        other => println!("  state {other:?}"),
                    }
                }
                Some(Obj::Instance { fields, .. }) => {
                    println!("result instance h={h} class={}", vm.class_descriptor_of(h).unwrap_or_default());
                    for (k, v) in fields {
                        if let V::Ref(x) = v {
                            println!("  field {k} -> h{x} {}", vm.class_descriptor_of(*x).unwrap_or_default());
                            match vm.heap().get(*x) {
                                Some(Obj::Host { state: HostState::List(l), .. }) => {
                                    println!("    list len={}", l.len());
                                }
                                other => println!("    {other:?}"),
                            }
                        }
                    }
                }
                other => println!("result obj h={h} {other:?}"),
            }
        }
        Ok(v) => println!("result {v:?}"),
        Err(e) => println!("ERROR {e}"),
    }
}
