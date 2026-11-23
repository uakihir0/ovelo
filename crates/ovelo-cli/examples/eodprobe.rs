use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_dex::DexFile;
use ovelo_dexrt::Obj;
use ovelo_dexrt::{V, Vm};
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
    let input = "WHERE (tasks.dueDate<='EOD()') AND (x='NOW()')";
    let s = V::Ref(vm.intern_str(input));
    match vm.run(
        "Lkotlin/ranges/RangesKt;",
        "replacePlaceholdersForQuery",
        &[s],
    ) {
        Ok(Some(v)) => match v {
            V::Ref(h) => match vm.heap().get(h) {
                Some(Obj::Str(t)) => println!("OUT = {t}"),
                other => println!("OUT OBJ = {other:?}"),
            },
            other => println!("OUT = {other:?}"),
        },
        Ok(None) => println!("OUT = void"),
        Err(e) => println!("ERR = {e}"),
    }
}
