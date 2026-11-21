//! dump instructions of methods whose name matches a substring
use ovelo_apk::zip::{parse_central_directory, read_entry};
use ovelo_dex::*;
fn main() {
    let apk = std::env::args().nth(1).unwrap();
    let pat = std::env::args().nth(2).unwrap();
    let owner_pat = std::env::args().nth(3).unwrap_or_default();
    let bytes = std::fs::read(&apk).unwrap();
    let (_cd, ents) = parse_central_directory(&bytes, &Default::default()).unwrap();
    for e in &ents {
        if !e.name.starts_with("classes") || !e.name.ends_with(".dex") {
            continue;
        }
        let buf = read_entry(&bytes, e).unwrap();
        let dex = match DexFile::parse(buf) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for cd in dex.class_defs() {
            let owner = dex.type_descriptor(cd.class_idx).unwrap_or_default();
            if !owner.contains(&owner_pat) {
                continue;
            }
            let Ok(data) = dex.class_data(cd) else {
                continue;
            };
            for m in data
                .direct_methods
                .iter()
                .chain(data.virtual_methods.iter())
            {
                let Ok(mm) = dex.method(m.method_idx) else {
                    continue;
                };
                let mn = dex.string(mm.name_idx).unwrap_or_default();
                let hit = if pat == "*" {
                    true
                } else if let Some(exact) = pat.strip_prefix('=') {
                    mn == exact
                } else {
                    mn.contains(&pat)
                };
                if !hit {
                    continue;
                }
                println!("== {owner}.{mn} proto={}", m.method_idx);
                let Ok(Some(code)) = dex.code_item(m.code_off) else {
                    continue;
                };
                let Ok(insns) = decode_all(&code.insns) else {
                    continue;
                };
                for (i, ins) in insns.iter().enumerate() {
                    let mut line = format!(
                        "  {i:04x}@{:04x} {} regs={:?}",
                        ins.offset, ins.name, ins.regs
                    );
                    if let Some(t) = ins.target {
                        let ti = insns
                            .iter()
                            .position(|x| x.offset == t)
                            .map(|x| format!("{x:04x}"))
                            .unwrap_or_else(|| format!("cu{t:04x}"));
                        line += &format!(" -> {ti}");
                    }
                    if let Some(ix) = ins.idx {
                        let n = ins.name;
                        if n.starts_with("iget")
                            || n.starts_with("iput")
                            || n.starts_with("sget")
                            || n.starts_with("sput")
                        {
                            if let Ok(f) = dex.field(ix) {
                                let fo =
                                    dex.type_descriptor(f.class_idx.into()).unwrap_or_default();
                                let fn_ = dex.string(f.name_idx).unwrap_or_default();
                                let ft = dex.type_descriptor(f.type_idx.into()).unwrap_or_default();
                                line += &format!(" -> field {fo}.{fn_}:{ft}");
                            }
                        } else if let Ok(tm) = dex.method(ix) {
                            let to = dex.type_descriptor(tm.class_idx.into()).unwrap_or_default();
                            let tn = dex.string(tm.name_idx).unwrap_or_default();
                            line += &format!(" -> {to}.{tn}");
                        }
                    }
                    println!("{line}");
                }
            }
        }
    }
}
