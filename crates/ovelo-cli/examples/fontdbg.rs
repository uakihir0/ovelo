//! list loaded font faces and glyph coverage for probe chars
fn main() {
    let mut fs = ovelo_ui::FontSet::new();
    let n = fs.load_dirs(&["/System/Library/Fonts", "/System/Library/Fonts/Supplemental"]);
    println!("loaded {} faces", n);
    for c in ['မ', 'န', 'ေ', 'ာ', 'က', 'A', '\u{200b}'] {
        println!("{:?} covered={:?}", c, fs.font_for(c, 16.0).map(|f| f.name().to_string()));
    }
}
