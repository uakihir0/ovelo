//! APK resources for the runtime.
//!
//! `resources.arsc` maps a numeric resource id (`0xPPTTEEEE`) to a file
//! inside the APK — `LayoutInflater.inflate` uses the `layout` entries to
//! load real binary XML, and the `id` entries resolve `@+id/name`
//! references found in that XML.

use hashbrown::HashMap;

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
        // Non-default config buckets accumulate in pending maps and are
        // merged after the walk. A bucket fully matching the device
        // config (every qualifier it carries is satisfied — e.g.
        // `w360dp` on a 360dp-wide screen) gets a positive key and
        // outranks `default`; a bucket with an unknown qualifier, or an
        // understood one that fails, returns 0 and stays a last-seen
        // fallback only for entries with no default.
        // Device: portrait 360x360dp (what Configuration reports),
        // API 35, NIGHT mode (Configuration.uiMode reports
        // UI_MODE_NIGHT_YES) — `night` buckets match, `notnight` fail.
        // Matched-key precedence follows AOSP: smallestScreenWidth >
        // screenWidthDp > screenHeightDp > other qualifiers; for each
        // dp qualifier the largest matching N wins. Among the misc
        // qualifiers, orientation outranks night (AOSP ordering).
        fn bucket_rank(config: &str) -> i64 {
            if config == "default" {
                return 0;
            }
            let (mut sw, mut w, mut h) = (0i64, 0i64, 0i64);
            let (mut has_sw, mut has_w, mut has_h) = (0i64, 0i64, 0i64);
            let mut misc = 0i64;
            for part in config.split('-') {
                let num = |p: &str, pre: &str, suf: &str| -> Option<u32> {
                    p.strip_prefix(pre)?.strip_suffix(suf)?.parse().ok()
                };
                let ok = if let Some(n) = num(part, "sw", "dp") {
                    has_sw = 1;
                    sw = n as i64;
                    n <= 360
                } else if let Some(n) = num(part, "w", "dp") {
                    has_w = 1;
                    w = n as i64;
                    n <= 360
                } else if let Some(n) = num(part, "h", "dp") {
                    has_h = 1;
                    h = n as i64;
                    n <= 360
                } else if let Some(n) = part.strip_prefix('v').and_then(|s| s.parse::<u32>().ok()) {
                    n <= 35
                } else {
                    match part {
                        "port" => {
                            misc += 4;
                            true
                        }
                        "night" => {
                            misc += 2;
                            true
                        }
                        "land" | "notnight" | "square" => false,
                        // Qualifiers we can't evaluate (lang, dpi,
                        // small/large, car/watch/tv) — keep the bucket
                        // as a fallback candidate only.
                        _ => return 0,
                    }
                };
                if !ok {
                    return 0; // understood qualifier, doesn't match
                }
            }
            // Keep bit 63 clear — the key must stay positive so a
            // fully-matching bucket sorts above every fallback (0).
            (has_sw << 62)
                | (sw << 45)
                | (has_w << 44)
                | (w << 27)
                | (has_h << 26)
                | (h << 9)
                | (misc << 1)
                | 1
        }
        let mut pend_values: HashMap<u32, (i64, EntryValue)> = HashMap::new();
        let mut pend_files: HashMap<u32, (i64, String)> = HashMap::new();
        let mut pend_layouts: HashMap<u32, (i64, String)> = HashMap::new();
        type PendStyle = (i64, (u32, Vec<(u32, EntryValue)>));
        let mut pend_styles: HashMap<u32, PendStyle> = HashMap::new();
        let mut pend_plurals: HashMap<u32, (i64, Vec<(String, EntryValue)>)> = HashMap::new();
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
                            let r = bucket_rank(&e.config);
                            match pend_values.get_mut(&e.res_id) {
                                Some((s, slot)) if r >= *s => {
                                    *s = r;
                                    *slot = v.clone();
                                }
                                Some(_) => {}
                                None => {
                                    pend_values.insert(e.res_id, (r, v.clone()));
                                }
                            }
                        }
                    }
                    // A complex (style) entry: either a parent or items
                    // marks it — a `<style name="X" parent="Y"/>` with no
                    // items still has to sit in the chain.
                    if e.parent != 0 || !e.items.is_empty() {
                        if e.config == "default" {
                            let s = res.styles.entry(e.res_id).or_default();
                            s.parent = e.parent;
                            s.items = e.items.clone();
                        } else {
                            let r = bucket_rank(&e.config);
                            match pend_styles.get_mut(&e.res_id) {
                                Some((s, slot)) if r >= *s => {
                                    *s = r;
                                    *slot = (e.parent, e.items.clone());
                                }
                                Some(_) => {}
                                None => {
                                    pend_styles.insert(e.res_id, (r, (e.parent, e.items.clone())));
                                }
                            }
                        }
                    }
                    if t.name == "plurals" && !e.items.is_empty() {
                        // Plural item keys are the reserved
                        // attr ids 0x01000004..0x01000009
                        // (ResTable_map ATTR_other/zero/…), not
                        // key-pool indices — a `key_strings.get`
                        // on those yields nothing.
                        const QTY_IDS: &[&str] = &["other", "zero", "one", "two", "few", "many"];
                        let slot: Vec<(String, EntryValue)> = e
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
                        if e.config == "default" {
                            res.plurals.insert(e.res_id, slot);
                        } else {
                            let r = bucket_rank(&e.config);
                            match pend_plurals.get_mut(&e.res_id) {
                                Some((s, sl)) if r >= *s => {
                                    *s = r;
                                    *sl = slot;
                                }
                                Some(_) => {}
                                None => {
                                    pend_plurals.insert(e.res_id, (r, slot));
                                }
                            }
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
                                let r = bucket_rank(&e.config);
                                match pend_files.get_mut(&e.res_id) {
                                    Some((s, sl)) if r >= *s => {
                                        *s = r;
                                        *sl = path.clone();
                                    }
                                    Some(_) => {}
                                    None => {
                                        pend_files.insert(e.res_id, (r, path.clone()));
                                    }
                                }
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
                                    let r = bucket_rank(&e.config);
                                    match pend_layouts.get_mut(&e.res_id) {
                                        Some((s, sl)) if r >= *s => {
                                            *s = r;
                                            *sl = path.clone();
                                        }
                                        Some(_) => {}
                                        None => {
                                            pend_layouts.insert(e.res_id, (r, path.clone()));
                                        }
                                    }
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
        // Merge qualified buckets: a bucket whose qualifiers all
        // matched (key > 0) overrides `default`; a fallback bucket
        // (key == 0) fills only entries with no default.
        for (k, (r, v)) in pend_values {
            if r > 0 {
                res.entry_values.insert(k, v);
            } else {
                res.entry_values.entry(k).or_insert(v);
            }
        }
        for (k, (r, v)) in pend_files {
            if r > 0 {
                res.file_paths.insert(k, v);
            } else {
                res.file_paths.entry(k).or_insert(v);
            }
        }
        for (k, (r, v)) in pend_layouts {
            if r > 0 {
                res.layout_paths.insert(k, v);
            } else {
                res.layout_paths.entry(k).or_insert(v);
            }
        }
        for (k, (r, (p, items))) in pend_styles {
            let s = res.styles.entry(k).or_default();
            if r > 0 || (s.parent == 0 && s.items.is_empty()) {
                s.parent = p;
                s.items = items;
            }
        }
        for (k, (r, v)) in pend_plurals {
            if r > 0 {
                res.plurals.insert(k, v);
            } else {
                res.plurals.entry(k).or_insert(v);
            }
        }
        Some(res)
    }

    /// A resource entry whose value is a reference (`@layout/foo`)
    /// resolves to the referenced res id.
    fn ref_target(&self, res_id: u32) -> Option<u32> {
        let v = self.entry_values.get(&res_id)?;
        (v.data_type == 0x01 && v.data != 0).then_some(v.data)
    }

    /// Follow the winning bucket's references until an id with a
    /// stored path is reached (bounded against reference cycles).
    fn resolve_path(
        paths: &HashMap<u32, String>,
        mut ref_target: impl FnMut(u32) -> Option<u32>,
        res_id: u32,
    ) -> Option<&str> {
        let mut cur = res_id;
        for _ in 0..16 {
            if let Some(t) = ref_target(cur) {
                cur = t;
                continue;
            }
            return paths.get(&cur).map(|s| s.as_str());
        }
        None
    }

    /// `layout` res_id → path inside the APK.
    ///
    /// The winning bucket's value decides: when it is a reference
    /// (`@layout/foo`), follow it — a `drawable-night`-style alias must
    /// not fall back to the default bucket's direct path.
    pub fn layout_path(&self, res_id: u32) -> Option<&str> {
        Self::resolve_path(&self.layout_paths, |id| self.ref_target(id), res_id)
    }

    /// Any file-backed res_id (`xml`, `menu`, `drawable`, …) → path
    /// inside the APK. Reference-following order matches `layout_path`.
    pub fn file_path(&self, res_id: u32) -> Option<&str> {
        Self::resolve_path(&self.file_paths, |id| self.ref_target(id), res_id)
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

    /// Named `attr` resource → res_id (e.g. `colorSurface` for a
    /// material attr the app defines).
    pub fn attr_id_named(&self, name: &str) -> Option<u32> {
        self.entry_names
            .iter()
            .find(|(id, n)| n.as_str() == name && self.type_name(**id) == Some("attr"))
            .map(|(id, _)| *id)
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

    /// res_id → its integer value, chasing `TYPE_REFERENCE` aliases.
    /// Android integer resources use TYPE_INT_DEC/TYPE_INT_HEX payloads.
    pub fn integer_of(&self, res_id: u32) -> Option<i64> {
        let mut cur = res_id;
        for _ in 0..8 {
            let v = self.entry_value(cur)?;
            match v.data_type {
                0x01 => cur = v.data,
                0x10 | 0x11 => return Some((v.data as i32) as i64),
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
/// `android.R.color` res-ids -> ARGB literals (AOSP defaults; dynamic
/// accent/neutral palettes use the Material baseline palette).
pub fn framework_color(res_id: u32) -> Option<u32> {
    match res_id {
        0x01060000 => Some(0xffaaaaaa), // darker_gray
        0x01060001 => Some(0xffffffff), // primary_text_dark
        0x01060002 => Some(0xffffffff), // primary_text_dark_nodisable
        0x01060003 => Some(0xde000000), // primary_text_light
        0x01060004 => Some(0xde000000), // primary_text_light_nodisable
        0x01060005 => Some(0xb3ffffff), // secondary_text_dark
        0x01060006 => Some(0xb3ffffff), // secondary_text_dark_nodisable
        0x01060007 => Some(0x8a000000), // secondary_text_light
        0x01060008 => Some(0x8a000000), // secondary_text_light_nodisable
        0x01060009 => Some(0x80808080), // tab_indicator_text
        0x0106000a => Some(0xff000000), // widget_edittext_dark
        0x0106000b => Some(0xffffffff), // white
        0x0106000c => Some(0xff000000), // black
        0x0106000d => Some(0x00000000), // transparent
        0x0106000e => Some(0xff000000), // background_dark
        0x0106000f => Some(0xffffffff), // background_light
        0x01060010 => Some(0x80ffffff), // tertiary_text_dark
        0x01060011 => Some(0x61000000), // tertiary_text_light
        0x01060012 => Some(0xff33b5e5), // holo_blue_light
        0x01060013 => Some(0xff0099cc), // holo_blue_dark
        0x01060014 => Some(0xff99cc00), // holo_green_light
        0x01060015 => Some(0xff669900), // holo_green_dark
        0x01060016 => Some(0xffff4444), // holo_red_light
        0x01060017 => Some(0xffcc0000), // holo_red_dark
        0x01060018 => Some(0xffffbb33), // holo_orange_light
        0x01060019 => Some(0xffff8800), // holo_orange_dark
        0x0106001a => Some(0xffaa66cc), // holo_purple
        0x0106001b => Some(0xff00ddff), // holo_blue_bright
        0x0106001c => Some(0x00000000), // system_notification_accent_color
        0x0106001d => Some(0xffffffff), // system_neutral1_0
        0x0106001e => Some(0xfffefbff), // system_neutral1_10
        0x0106001f => Some(0xfff1f0f7), // system_neutral1_50
        0x01060020 => Some(0xffe2e2e9), // system_neutral1_100
        0x01060021 => Some(0xffc6c6cd), // system_neutral1_200
        0x01060022 => Some(0xffababb1), // system_neutral1_300
        0x01060023 => Some(0xff909097), // system_neutral1_400
        0x01060024 => Some(0xff76777d), // system_neutral1_500
        0x01060025 => Some(0xff5d5e64), // system_neutral1_600
        0x01060026 => Some(0xff45464c), // system_neutral1_700
        0x01060027 => Some(0xff2f3036), // system_neutral1_800
        0x01060028 => Some(0xff1a1b20), // system_neutral1_900
        0x01060029 => Some(0xff000000), // system_neutral1_1000
        0x0106002a => Some(0xffffffff), // system_neutral2_0
        0x0106002b => Some(0xfffefbff), // system_neutral2_10
        0x0106002c => Some(0xfff0f0fa), // system_neutral2_50
        0x0106002d => Some(0xffe1e2ec), // system_neutral2_100
        0x0106002e => Some(0xffc5c6d0), // system_neutral2_200
        0x0106002f => Some(0xffa9abb4), // system_neutral2_300
        0x01060030 => Some(0xff8f9099), // system_neutral2_400
        0x01060031 => Some(0xff757780), // system_neutral2_500
        0x01060032 => Some(0xff5c5e67), // system_neutral2_600
        0x01060033 => Some(0xff44464f), // system_neutral2_700
        0x01060034 => Some(0xff2e3038), // system_neutral2_800
        0x01060035 => Some(0xff191b23), // system_neutral2_900
        0x01060036 => Some(0xff000000), // system_neutral2_1000
        0x01060037 => Some(0xffffffff), // system_accent1_0
        0x01060038 => Some(0xfffefbff), // system_accent1_10
        0x01060039 => Some(0xffeef0ff), // system_accent1_50
        0x0106003a => Some(0xffd9e2ff), // system_accent1_100
        0x0106003b => Some(0xffb0c6ff), // system_accent1_200
        0x0106003c => Some(0xff94aae4), // system_accent1_300
        0x0106003d => Some(0xff7a90c8), // system_accent1_400
        0x0106003e => Some(0xff6076ac), // system_accent1_500
        0x0106003f => Some(0xff475d92), // system_accent1_600
        0x01060040 => Some(0xff2f4578), // system_accent1_700
        0x01060041 => Some(0xff152e60), // system_accent1_800
        0x01060042 => Some(0xff001945), // system_accent1_900
        0x01060043 => Some(0xff000000), // system_accent1_1000
        0x01060044 => Some(0xffffffff), // system_accent2_0
        0x01060045 => Some(0xfffefbff), // system_accent2_10
        0x01060046 => Some(0xffeef0ff), // system_accent2_50
        0x01060047 => Some(0xffdce2f9), // system_accent2_100
        0x01060048 => Some(0xffc0c6dc), // system_accent2_200
        0x01060049 => Some(0xffa4abc1), // system_accent2_300
        0x0106004a => Some(0xff8a90a5), // system_accent2_400
        0x0106004b => Some(0xff70778b), // system_accent2_500
        0x0106004c => Some(0xff575e71), // system_accent2_600
        0x0106004d => Some(0xff404659), // system_accent2_700
        0x0106004e => Some(0xff2a3042), // system_accent2_800
        0x0106004f => Some(0xff151b2c), // system_accent2_900
        0x01060050 => Some(0xff000000), // system_accent2_1000
        0x01060051 => Some(0xffffffff), // system_accent3_0
        0x01060052 => Some(0xfffffbff), // system_accent3_10
        0x01060053 => Some(0xffffebfa), // system_accent3_50
        0x01060054 => Some(0xfffdd7fa), // system_accent3_100
        0x01060055 => Some(0xffe0bbdd), // system_accent3_200
        0x01060056 => Some(0xffc3a0c1), // system_accent3_300
        0x01060057 => Some(0xffa886a6), // system_accent3_400
        0x01060058 => Some(0xff8c6d8c), // system_accent3_500
        0x01060059 => Some(0xff725572), // system_accent3_600
        0x0106005a => Some(0xff593d59), // system_accent3_700
        0x0106005b => Some(0xff412742), // system_accent3_800
        0x0106005c => Some(0xff2a122c), // system_accent3_900
        0x0106005d => Some(0xff000000), // system_accent3_1000
        0x0106005e => Some(0xffd9e2ff), // system_primary_container_light
        0x0106005f => Some(0xff2f4578), // system_on_primary_container_light
        0x01060060 => Some(0xff475d92), // system_primary_light
        0x01060061 => Some(0xffffffff), // system_on_primary_light
        0x01060062 => Some(0xffdce2f9), // system_secondary_container_light
        0x01060063 => Some(0xff404659), // system_on_secondary_container_light
        0x01060064 => Some(0xff575e71), // system_secondary_light
        0x01060065 => Some(0xffffffff), // system_on_secondary_light
        0x01060066 => Some(0xfffdd7fa), // system_tertiary_container_light
        0x01060067 => Some(0xff593d59), // system_on_tertiary_container_light
        0x01060068 => Some(0xff725572), // system_tertiary_light
        0x01060069 => Some(0xffffffff), // system_on_tertiary_light
        0x0106006a => Some(0xfffaf8ff), // system_background_light
        0x0106006b => Some(0xff1a1b20), // system_on_background_light
        0x0106006c => Some(0xfffaf8ff), // system_surface_light
        0x0106006d => Some(0xff1a1b20), // system_on_surface_light
        0x0106006e => Some(0xfff4f3fa), // system_surface_container_low_light
        0x0106006f => Some(0xffffffff), // system_surface_container_lowest_light
        0x01060070 => Some(0xffeeedf4), // system_surface_container_light
        0x01060071 => Some(0xffe8e7ef), // system_surface_container_high_light
        0x01060072 => Some(0xffe2e2e9), // system_surface_container_highest_light
        0x01060073 => Some(0xfffaf8ff), // system_surface_bright_light
        0x01060074 => Some(0xffdad9e0), // system_surface_dim_light
        0x01060075 => Some(0xffe1e2ec), // system_surface_variant_light
        0x01060076 => Some(0xff44464f), // system_on_surface_variant_light
        0x01060077 => Some(0xff757780), // system_outline_light
        0x01060078 => Some(0xffba1a1a), // system_error_light
        0x01060079 => Some(0xffffffff), // system_on_error_light
        0x0106007a => Some(0xffffdad6), // system_error_container_light
        0x0106007b => Some(0xff93000a), // system_on_error_container_light
        0x0106007c => Some(0xffd9e2ff), // system_control_activated_light
        0x0106007d => Some(0xff44464f), // system_control_normal_light
        0x0106007e => Some(0xff000000), // system_control_highlight_light
        0x0106007f => Some(0xffe2e2e9), // system_text_primary_inverse_light
        0x01060080 => Some(0xffc5c6d0), // system_text_secondary_and_tertiary_inverse_light
        0x01060081 => Some(0xffe2e2e9), // system_text_primary_inverse_disable_only_light
        0x01060082 => Some(0xffe2e2e9), // system_text_secondary_and_tertiary_inverse_disabled_light
        0x01060083 => Some(0xffe2e2e9), // system_text_hint_inverse_light
        0x01060084 => Some(0xff6076ac), // system_palette_key_color_primary_light
        0x01060085 => Some(0xff70778b), // system_palette_key_color_secondary_light
        0x01060086 => Some(0xff8c6d8c), // system_palette_key_color_tertiary_light
        0x01060087 => Some(0xff76777d), // system_palette_key_color_neutral_light
        0x01060088 => Some(0xff757780), // system_palette_key_color_neutral_variant_light
        0x01060089 => Some(0xff2f4578), // system_primary_container_dark
        0x0106008a => Some(0xffd9e2ff), // system_on_primary_container_dark
        0x0106008b => Some(0xffb0c6ff), // system_primary_dark
        0x0106008c => Some(0xff152e60), // system_on_primary_dark
        0x0106008d => Some(0xff404659), // system_secondary_container_dark
        0x0106008e => Some(0xffdce2f9), // system_on_secondary_container_dark
        0x0106008f => Some(0xffc0c6dc), // system_secondary_dark
        0x01060090 => Some(0xff2a3042), // system_on_secondary_dark
        0x01060091 => Some(0xff593d59), // system_tertiary_container_dark
        0x01060092 => Some(0xfffdd7fa), // system_on_tertiary_container_dark
        0x01060093 => Some(0xffe0bbdd), // system_tertiary_dark
        0x01060094 => Some(0xff412742), // system_on_tertiary_dark
        0x01060095 => Some(0xff121318), // system_background_dark
        0x01060096 => Some(0xffe2e2e9), // system_on_background_dark
        0x01060097 => Some(0xff121318), // system_surface_dark
        0x01060098 => Some(0xffe2e2e9), // system_on_surface_dark
        0x01060099 => Some(0xff1a1b20), // system_surface_container_low_dark
        0x0106009a => Some(0xff0c0e13), // system_surface_container_lowest_dark
        0x0106009b => Some(0xff1e1f25), // system_surface_container_dark
        0x0106009c => Some(0xff282a2f), // system_surface_container_high_dark
        0x0106009d => Some(0xff33343a), // system_surface_container_highest_dark
        0x0106009e => Some(0xff38393f), // system_surface_bright_dark
        0x0106009f => Some(0xff121318), // system_surface_dim_dark
        0x010600a0 => Some(0xff44464f), // system_surface_variant_dark
        0x010600a1 => Some(0xffc5c6d0), // system_on_surface_variant_dark
        0x010600a2 => Some(0xff8f9099), // system_outline_dark
        0x010600a3 => Some(0xffffb4ab), // system_error_dark
        0x010600a4 => Some(0xff690005), // system_on_error_dark
        0x010600a5 => Some(0xff93000a), // system_error_container_dark
        0x010600a6 => Some(0xffffdad6), // system_on_error_container_dark
        0x010600a7 => Some(0xff2f4578), // system_control_activated_dark
        0x010600a8 => Some(0xffc5c6d0), // system_control_normal_dark
        0x010600a9 => Some(0xffffffff), // system_control_highlight_dark
        0x010600aa => Some(0xff1a1b20), // system_text_primary_inverse_dark
        0x010600ab => Some(0xff44464f), // system_text_secondary_and_tertiary_inverse_dark
        0x010600ac => Some(0xff1a1b20), // system_text_primary_inverse_disable_only_dark
        0x010600ad => Some(0xff1a1b20), // system_text_secondary_and_tertiary_inverse_disabled_dark
        0x010600ae => Some(0xff1a1b20), // system_text_hint_inverse_dark
        0x010600af => Some(0xff6076ac), // system_palette_key_color_primary_dark
        0x010600b0 => Some(0xff70778b), // system_palette_key_color_secondary_dark
        0x010600b1 => Some(0xff8c6d8c), // system_palette_key_color_tertiary_dark
        0x010600b2 => Some(0xff76777d), // system_palette_key_color_neutral_dark
        0x010600b3 => Some(0xff757780), // system_palette_key_color_neutral_variant_dark
        0x010600b4 => Some(0xffd9e2ff), // system_primary_fixed
        0x010600b5 => Some(0xffb0c6ff), // system_primary_fixed_dim
        0x010600b6 => Some(0xff001945), // system_on_primary_fixed
        0x010600b7 => Some(0xff2f4578), // system_on_primary_fixed_variant
        0x010600b8 => Some(0xffdce2f9), // system_secondary_fixed
        0x010600b9 => Some(0xffc0c6dc), // system_secondary_fixed_dim
        0x010600ba => Some(0xff151b2c), // system_on_secondary_fixed
        0x010600bb => Some(0xff404659), // system_on_secondary_fixed_variant
        0x010600bc => Some(0xfffdd7fa), // system_tertiary_fixed
        0x010600bd => Some(0xffe0bbdd), // system_tertiary_fixed_dim
        0x010600be => Some(0xff2a122c), // system_on_tertiary_fixed
        0x010600bf => Some(0xff593d59), // system_on_tertiary_fixed_variant
        0x010600c0 => Some(0xffc5c6d0), // system_outline_variant_light
        0x010600c1 => Some(0xff44464f), // system_outline_variant_dark
        0x010600c2 => Some(0x1f1b1b22), // system_surface_disabled
        0x010600c3 => Some(0x1fe1e3e8), // system_on_surface_disabled
        0x010600c4 => Some(0x1f757680), // system_outline_disabled
        0x010600c5 => Some(0xffffffff), // system_error_0
        0x010600c6 => Some(0xfffffbff), // system_error_10
        0x010600c7 => Some(0xffffedea), // system_error_50
        0x010600c8 => Some(0xffffdad6), // system_error_100
        0x010600c9 => Some(0xffffb4ab), // system_error_200
        0x010600ca => Some(0xffff897d), // system_error_300
        0x010600cb => Some(0xffff5449), // system_error_400
        0x010600cc => Some(0xffde3730), // system_error_500
        0x010600cd => Some(0xffba1a1a), // system_error_600
        0x010600ce => Some(0xff93000a), // system_error_700
        0x010600cf => Some(0xff690005), // system_error_800
        0x010600d0 => Some(0xff410002), // system_error_900
        0x010600d1 => Some(0xff000000), // system_error_1000
        _ => None,
    }
}
