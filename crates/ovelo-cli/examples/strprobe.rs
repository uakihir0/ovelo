//! Probe kotlin.text.StringsKt dex functions directly.
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
use ovelo_dexrt::{Obj, Vm, V};
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &ZipLimits::default()).unwrap();
    let mut vm: Option<Vm> = None;
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let buf = read_entry(&bytes, e).unwrap();
        let Ok(d) = DexFile::parse(buf) else { continue };
        match &mut vm {
            None => vm = Some(Vm::new(d).unwrap()),
            Some(v) => v.add_dex(d).unwrap(),
        }
    }
    let mut vm = vm.unwrap();
    let inp = vm.intern_str("string|filter_my_tasks|TXkgVGFza3M=");
    let chars = vm.alloc_array("C", vec![V::I32(124)]);
    for nargs in 2..=6 {
        let mut args = vec![V::Ref(inp), chars];
        args.extend(std::iter::repeat_n(V::I32(0), nargs - 2));
        let r = vm.run("Lkotlin/text/StringsKt;", "split$default", &args);
        match r {
            Ok(Some(V::Ref(h))) => match vm.heap().get(h) {
                Some(Obj::Host {
                    state: ovelo_dexrt::HostState::List(l),
                    ..
                }) => {
                    print!("nargs={nargs}: [");
                    for v in l.iter() {
                        let s = match v {
                            V::Ref(sh) => match vm.heap().get(*sh) {
                                Some(Obj::Str(t)) => t.clone(),
                                other => format!("{other:?}"),
                            },
                            other => format!("{other:?}"),
                        };
                        print!("{s:?} ");
                    }
                    println!("]");
                }
                other => println!("nargs={nargs}: {other:?}"),
            },
            other => {
                let e = format!("{other:?}");
                println!("nargs={nargs}: {}", &e[..e.len().min(160)]);
            }
        }
    }
}
