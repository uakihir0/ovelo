//! APK resources for the runtime.
//!
//! `resources.arsc` maps a numeric resource id (`0xPPTTEEEE`) to a file
//! inside the APK — `LayoutInflater.inflate` uses the `layout` entries to
//! load real binary XML, and the `id` entries resolve `@+id/name`
//! references found in that XML.

use std::collections::HashMap;

use ovelo_apk::arsc::{parse_arsc, EntryValue};
use ovelo_apk::manifest::{parse_axml, Element};
use ovelo_apk::zip::{parse_central_directory, read_entry, CentralEntry, ZipLimits};

use crate::res_attrs::framework_attr_name;

/// A style resource: parent style id plus its attribute items.
#[derive(Default)]
struct StyleDef {
    parent: u32,
    /// Attr id → value, in arsc document order: array items reuse the
    /// same key (or key 0) so a map would drop repeats and ordering.
    items: Vec<(u32, EntryValue)>,
}

/// Resource index of the installed APK. Built once per `Vm` from the APK
/// bytes; lookups never touch the filesystem.
pub struct ApkResources {
    bytes: Vec<u8>,
    entries: Vec<CentralEntry>,
    /// res_id → zip path (`res/layout/…`) for `layout` entries.
    layout_paths: HashMap<u32, String>,
    /// res_id → zip path for every file-backed entry (`xml`, `menu`,
    /// `drawable`, …), for `Resources.getXml` and `AssetManager`.
    file_paths: HashMap<u32, String>,
    /// res_id → entry key name (any type) for `getResourceEntryName`.
    entry_names: HashMap<u32, String>,
    /// `id`-type entry name → res_id, for `@+id/name` / `@id/name`.
    id_by_name: HashMap<String, u32>,
    /// res_id → type name (`id`, `layout`, …) for `getResourceTypeName`.
    type_names: HashMap<u32, String>,
    /// Package name of the app resource package (for `getResourceName`).
    package_name: String,
    /// res_id → simple value (default config preferred).
    entry_values: HashMap<u32, EntryValue>,
    /// style res_id → parent + items, for theme attribute resolution.
    styles: HashMap<u32, StyleDef>,
    /// `<plurals>` res_id → (quantity name, value) items —
    /// `getQuantityString` picks the AOSP bucket for the count.
    plurals: HashMap<u32, Vec<(String, EntryValue)>>,
}

impl ApkResources {
    /// Build the index from raw APK bytes. Malformed/missing `resources.arsc`
    /// yields an index that simply resolves nothing (inflation degrades to
    /// placeholder views instead of failing the run).
    pub fn open(bytes: Vec<u8>) -> Option<Self> {
        let (_cd, entries) = parse_central_directory(&bytes, &ZipLimits::default()).ok()?;
        let mut res = Self {
            bytes,
            entries,
            layout_paths: HashMap::new(),
            file_paths: HashMap::new(),
            entry_names: HashMap::new(),
            id_by_name: HashMap::new(),
            type_names: HashMap::new(),
            package_name: String::new(),
            entry_values: HashMap::new(),
            styles: HashMap::new(),
            plurals: HashMap::new(),
        };
        let arsc = res.read("resources.arsc")?;
        for pkg in parse_arsc(&arsc).ok()? {
            if pkg.package_id & 0xff == 0x7f {
                res.package_name = pkg.name.clone();
            }
            for t in &pkg.types {
                for e in &t.entries {
                    res.type_names
                        .entry(e.res_id)
                        .or_insert_with(|| t.name.clone());
                    res.entry_names
                        .entry(e.res_id)
                        .or_insert_with(|| e.key.clone());
                    if let Some(v) = &e.value {
                        if e.config == "default" {
                            res.entry_values.insert(e.res_id, v.clone());
                        } else {
                            res.entry_values
                                .entry(e.res_id)
                                .or_insert_with(|| v.clone());
                        }
                    }
                    // A complex (style) entry: either a parent or items
                    // marks it — a `<style name="X" parent="Y"/>` with no
                    // items still has to sit in the chain.
                    if e.parent != 0 || !e.items.is_empty() {
                        let s = res.styles.entry(e.res_id).or_default();
                        if e.config == "default" || (s.parent == 0 && s.items.is_empty()) {
                            s.parent = e.parent;
                            s.items = e.items.clone();
                        }
                    }
                    if t.name == "plurals" && !e.items.is_empty() {
                        let slot = res.plurals.entry(e.res_id).or_default();
                        if slot.is_empty() || e.config == "default" {
                            // Plural item keys are the reserved
                            // attr ids 0x01000004..0x01000009
                            // (ResTable_map ATTR_other/zero/…), not
                            // key-pool indices — a `key_strings.get`
                            // on those yields nothing.
                            const QTY_IDS: &[&str] =
                                &["other", "zero", "one", "two", "few", "many"];
                            *slot = e
                                .items
                                .iter()
                                .map(|(k, v)| {
                                    let key = if (0x01000004..=0x01000009).contains(k) {
                                        QTY_IDS[(*k - 0x01000004) as usize].to_string()
                                    } else {
                                        pkg.key_strings
                                            .get(*k as usize)
                                            .cloned()
                                            .unwrap_or_default()
                                    };
                                    (key, v.clone())
                                })
                                .collect();
                        }
                    }
                    // Any entry whose value is a `res/` file path —
                    // layout/xml/menu/drawable/… — is reachable by id for
                    // `Resources.getXml`/`getLayout` and friends.
                    if let Some(path) = e.value.as_ref().and_then(|v| v.string.as_ref()) {
                        if path.starts_with("res/") {
                            if e.config == "default" {
                                res.file_paths.insert(e.res_id, path.clone());
                            } else {
                                res.file_paths
                                    .entry(e.res_id)
                                    .or_insert_with(|| path.clone());
                            }
                        }
                    }
                    match t.name.as_str() {
                        "layout" => {
                            if let Some(path) = e.value.as_ref().and_then(|v| v.string.as_ref()) {
                                // Prefer the default config bucket; otherwise
                                // first seen wins (real config matching is a
                                // later milestone).
                                if e.config == "default" {
                                    res.layout_paths.insert(e.res_id, path.clone());
                                } else {
                                    res.layout_paths
                                        .entry(e.res_id)
                                        .or_insert_with(|| path.clone());
                                }
                            }
                        }
                        "id" => {
                            res.id_by_name.entry(e.key.clone()).or_insert(e.res_id);
                        }
                        _ => {}
                    }
                }
            }
        }
        Some(res)
    }

    /// `layout` res_id → path inside the APK.
    pub fn layout_path(&self, res_id: u32) -> Option<&str> {
        self.layout_paths.get(&res_id).map(|s| s.as_str())
    }

    /// Any file-backed res_id (`xml`, `menu`, `drawable`, …) → path
    /// inside the APK.
    pub fn file_path(&self, res_id: u32) -> Option<&str> {
        self.file_paths.get(&res_id).map(|s| s.as_str())
    }

    /// Parse a file-backed resource's binary XML into flat
    /// start-elements (depth-sorted); `None` when it isn't binary XML.
    pub fn document(&self, res_id: u32) -> Option<Vec<Element>> {
        let xml = self.read(self.file_path(res_id)?)?;
        parse_axml(&xml).ok()
    }

    /// Named `id` resource → res_id.
    pub fn id_named(&self, name: &str) -> Option<u32> {
        self.id_by_name.get(name).copied()
    }

    /// res_id → entry key name.
    pub fn entry_name(&self, res_id: u32) -> Option<&str> {
        self.entry_names.get(&res_id).map(|s| s.as_str())
    }

    /// The app resource package's name, when known.
    pub fn package_name(&self) -> Option<&str> {
        (!self.package_name.is_empty()).then_some(self.package_name.as_str())
    }

    /// res_id → type name (`id`, `layout`, …).
    pub fn type_name(&self, res_id: u32) -> Option<&str> {
        self.type_names.get(&res_id).map(|s| s.as_str())
    }

    /// Fully-qualified `pkg:type/name` for `Resources.getResourceName`;
    /// falls back to the bare entry name when the type is unknown.
    pub fn resource_name(&self, res_id: u32) -> Option<String> {
        let name = self.entry_name(res_id)?;
        let ty = self.type_name(res_id).unwrap_or("");
        let pkg = if self.package_name.is_empty() {
            "app"
        } else {
            &self.package_name
        };
        Some(format!("{pkg}:{ty}/{name}"))
    }

    /// res_id → entry key name, falling back to the framework attr table
    /// for `android:` ids (0x0101xxxx) that the app's arsc doesn't carry.
    pub fn attr_name(&self, res_id: u32) -> Option<&str> {
        self.entry_name(res_id)
            .or_else(|| framework_attr_name(res_id))
    }

    /// res_id → the entry's typed value (default config).
    pub fn entry_value(&self, res_id: u32) -> Option<&EntryValue> {
        self.entry_values.get(&res_id)
    }

    /// res_id → its string value when the entry is `TYPE_STRING`,
    /// chasing `TYPE_REFERENCE` aliases (a string may point at another
    /// string entry — `Resources.getString` on the alias returns it).
    pub fn string_of(&self, res_id: u32) -> Option<&str> {
        let mut cur = res_id;
        for _ in 0..8 {
            let v = self.entry_value(cur)?;
            match v.data_type {
                0x03 => return v.string.as_deref(),
                0x01 => cur = v.data,
                _ => return None,
            }
        }
        None
    }

    /// `attr_id` inside `style_id` or any style in its parent chain —
    /// the theme-resolution walk `obtainStyledAttributes` performs.
    /// A style whose parent isn't a complex entry stops the walk rather
    /// than dropping everything found so far.
    pub fn style_item(&self, style_id: u32, attr_id: u32) -> Option<&EntryValue> {
        let mut cur = style_id;
        for _ in 0..32 {
            let s = self.styles.get(&cur)?;
            if let Some((_, v)) = s.items.iter().find(|(k, _)| *k == attr_id) {
                return Some(v);
            }
            match s.parent {
                0 => return None,
                p => cur = p,
            }
        }
        None
    }

    /// Parent style res_id, for debugging resolution chains.
    pub fn style_parent(&self, style_id: u32) -> Option<u32> {
        self.styles.get(&style_id).map(|s| s.parent)
    }

    /// `<plurals>` entry → the item for `quantity`. CLDR plural rules
    /// are locale-dependent — with no locale model the default is
    /// English (`one` when exactly 1, else `other`); "other" is the
    /// always-required fallback.
    pub fn plural_string(&self, res_id: u32, quantity: i64) -> Option<&EntryValue> {
        let items = self.plurals.get(&res_id)?;
        let want = if quantity == 1 { "one" } else { "other" };
        items
            .iter()
            .find(|(k, _)| k == want)
            .or_else(|| items.iter().find(|(k, _)| k == "other"))
            .or_else(|| items.iter().find(|(k, _)| k == "many"))
            .or(items.first())
            .map(|(_, v)| v)
    }

    /// A complex entry's item values in document order — `<array>` and
    /// `<string-array>` elements for `getTextArray`/`getIntArray`.
    pub fn array_items(&self, res_id: u32) -> Vec<EntryValue> {
        self.styles
            .get(&res_id)
            .map(|s| s.items.iter().map(|(_, v)| v.clone()).collect())
            .unwrap_or_default()
    }

    /// All attr ids a style (not its parents) carries — debugging aid.
    pub fn style_attrs(&self, style_id: u32) -> Vec<(u32, EntryValue)> {
        self.styles
            .get(&style_id)
            .map(|s| s.items.clone())
            .unwrap_or_default()
    }

    /// res_id → decoded `TYPE_DIMENSION`/`TYPE_FRACTION` value scaled to
    /// px (density 2.0, matching the `scaledDensity` stub), or plain
    /// numeric data for int/float values. `TYPE_REFERENCE` aliases are
    /// chased — a `<dimen>` may point at another entry; a chain that
    /// lands on a non-numeric type means the id isn't a dimension and
    /// must not be read as one (its raw id would pass through as px).
    pub fn dimen(&self, res_id: u32) -> Option<f32> {
        let mut cur = res_id;
        for _ in 0..8 {
            let v = self.entry_value(cur)?;
            match v.data_type {
                0x01 => cur = v.data, // TYPE_REFERENCE
                0x02 => return None,  // TYPE_ATTRIBUTE — theme ref
                0x03 => return None,  // TYPE_STRING
                _ => return Some(typed_px(v)),
            }
        }
        None
    }

    /// Read a file out of the APK zip by its stored path.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let e = self.entries.iter().find(|e| e.name == path)?;
        read_entry(&self.bytes, e).ok()
    }

    /// Parse a layout's binary XML into flat start-elements (depth-sorted).
    pub fn layout_elements(&self, res_id: u32) -> Option<Vec<Element>> {
        let xml = self.read(self.layout_path(res_id)?)?;
        parse_axml(&xml).ok()
    }
}

/// AOSP `Res_value::complexToFloat`.
fn complex_to_float(data: u32) -> f32 {
    const RADIX_MULTS: [f32; 4] = [0.00390625, 3.051_758e-5, 1.192_093e-7, 4.656_613e-10];
    (data & 0xffff_ff00) as i32 as f32 * RADIX_MULTS[((data >> 4) & 0x3) as usize]
}

/// A `Res_value` as px-ish float: dimensions/fractions decode through
/// `complexToFloat` scaled by unit (density 2.0), ints/floats cast.
pub fn typed_px(v: &EntryValue) -> f32 {
    const DENSITY: f32 = 2.0;
    match v.data_type {
        0x05 => {
            // TYPE_DIMENSION: px=1, dip/sp=density, pt/in/mm scaled.
            let scale = match v.data & 0xf {
                0 => 1.0,
                1 | 2 => DENSITY,
                3 => DENSITY / 72.0,
                4 => DENSITY,
                5 => DENSITY / 25.4,
                _ => 1.0,
            };
            complex_to_float(v.data) * scale
        }
        0x06 => complex_to_float(v.data) * 100.0, // TYPE_FRACTION → %
        0x04 => f32::from_bits(v.data),
        _ => v.data as i32 as f32,
    }
}
