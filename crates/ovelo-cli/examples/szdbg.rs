fn main() {
    println!("Obj={} HostState={} V={}",
        std::mem::size_of::<ovelo_dexrt::Obj>(),
        std::mem::size_of::<ovelo_dexrt::HostState>(),
        std::mem::size_of::<ovelo_dexrt::V>());
}
