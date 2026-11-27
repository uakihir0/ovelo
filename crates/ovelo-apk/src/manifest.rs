//! Binary `AndroidManifest.xml` (AXML) parsing (design doc §03 step 2).

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
const RES_XML_CDATA_TYPE: u16 = 0x0104;

const UTF8_FLAG: u32 = 0x0000_0100;

const TYPE_NULL: u8 = 0x00;
const TYPE_REFERENCE: u8 = 0x01;
const TYPE_ATTRIBUTE: u8 = 0x02;
const TYPE_STRING: u8 = 0x03;
const TYPE_FLOAT: u8 = 0x04;
const TYPE_DIMENSION: u8 = 0x05;
const TYPE_FRACTION: u8 = 0x06;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;

/// Well-known `android:` attribute resource ids (public.xml of framework-res).
pub mod attr_id {
    /// android:name (0x01010003)
    pub const NAME: u32 = 0x0101_0003;
    /// android:versionCode
    pub const VERSION_CODE: u32 = 0x0101_021b;
    /// android:versionName
    pub const VERSION_NAME: u32 = 0x0101_021c;
    /// android:minSdkVersion
    pub const MIN_SDK: u32 = 0x0101_020c;
    /// android:targetSdkVersion
    pub const TARGET_SDK: u32 = 0x0101_0270;
    /// android:required
    pub const REQUIRED: u32 = 0x0101_028e;
}

/// android: attribute namespace URI.
const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";

fn err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

struct R<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.b.len());
        let end = end.ok_or_else(|| err("axml.truncated"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
}

/// A typed attribute value from binary XML.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Reference to a string in the string pool.
    Str(String),
    /// Decimal integer.
    Int(i64),
    /// Boolean.
    Bool(bool),
    /// Resource reference (e.g. @0x7f010001).
    Reference(u32),
    /// Float.
    Float(f32),
    /// Typed value we do not decode further.
    Other {
        /// AXML `Res_value` data type byte.
        data_type: u8,
        /// Raw 32-bit payload.
        data: u32,
    },
    /// No value.
    Null,
}

/// One XML attribute with resolved name.
#[derive(Debug, Clone)]
pub struct Attribute {
    /// Attribute local name.
    pub name: String,
    /// Namespace URI (may be empty).
    pub ns: String,
    /// Decoded value.
    pub value: Value,
}

/// One start-element event with attributes.
#[derive(Debug, Clone)]
pub struct Element {
    /// Element local name.
    pub name: String,
    /// Attributes.
    pub attrs: Vec<Attribute>,
    /// Depth in the document (root element is 0).
    pub depth: usize,
}

impl Element {
    /// Get an android-namespaced attribute by local name.
    pub fn android(&self, name: &str) -> Option<&Value> {
        self.attrs
            .iter()
            .find(|a| a.name == name && (a.ns == ANDROID_NS || a.ns.is_empty()))
            .map(|a| &a.value)
    }

    /// Convenience string accessor.
    pub fn android_str(&self, name: &str) -> Option<&str> {
        match self.android(name) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// Convenience int accessor.
    pub fn android_int(&self, name: &str) -> Option<i64> {
        match self.android(name) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }
}

fn read_utf16_len8(b: &[u8], pos: usize) -> Result<(usize, usize)> {
    // UTF-16 length: one u16; if high bit set, two u16s.
    if pos + 2 > b.len() {
        return Err(err("axml.stringpool.truncated"));
    }
    let first = u16::from_le_bytes(b[pos..pos + 2].try_into().unwrap());
    if first & 0x8000 != 0 {
        if pos + 4 > b.len() {
            return Err(err("axml.stringpool.truncated"));
        }
        let second = u16::from_le_bytes(b[pos + 2..pos + 4].try_into().unwrap());
        Ok((((first as usize & 0x7fff) << 16) | second as usize, 4))
    } else {
        Ok((first as usize, 2))
    }
}

fn read_utf8_len8(b: &[u8], pos: usize) -> Result<(usize, usize)> {
    // UTF-8 encoded length: utf16 len byte(s) then utf8 len byte(s).
    if pos + 1 > b.len() {
        return Err(err("axml.stringpool.truncated"));
    }
    let mut p = pos;
    let first = b[p];
    let utf16_len;
    if first & 0x80 != 0 {
        if p + 2 > b.len() {
            return Err(err("axml.stringpool.truncated"));
        }
        utf16_len = (((first & 0x7f) as usize) << 8) | b[p + 1] as usize;
        p += 2;
    } else {
        utf16_len = first as usize;
        p += 1;
    }
    let _ = utf16_len;
    if p + 1 > b.len() {
        return Err(err("axml.stringpool.truncated"));
    }
    let first = b[p];
    let utf8_len;
    if first & 0x80 != 0 {
        if p + 2 > b.len() {
            return Err(err("axml.stringpool.truncated"));
        }
        utf8_len = (((first & 0x7f) as usize) << 8) | b[p + 1] as usize;
        p += 2;
    } else {
        utf8_len = first as usize;
        p += 1;
    }
    Ok((utf8_len, p - pos))
}

/// A decoded string pool.
#[derive(Debug, Default)]
pub struct StringPool {
    strings: Vec<String>,
}

impl StringPool {
    /// Look up string by index.
    pub fn get(&self, idx: u32) -> Option<&str> {
        self.strings.get(idx as usize).map(|s| s.as_str())
    }

    fn parse(b: &[u8], chunk_start: usize, header_size: usize, chunk_size: usize) -> Result<Self> {
        let mut r = R::new(&b[chunk_start..chunk_start + chunk_size]);
        r.skip(8)?; // chunk header
        let string_count = r.u32()? as usize;
        let style_count = r.u32()? as usize;
        let flags = r.u32()?;
        let strings_start = r.u32()? as usize;
        let _styles_start = r.u32()?;

        if string_count > 1 << 24 {
            return Err(err("axml.stringpool.too_many"));
        }
        let header_end = header_size - 8;
        if r.pos + (string_count + style_count) * 4 > b[chunk_start..].len().min(chunk_size) {
            return Err(err("axml.stringpool.truncated"));
        }
        let mut offsets = Vec::with_capacity(string_count);
        for _ in 0..string_count {
            offsets.push(r.u32()? as usize);
        }
        r.skip(style_count * 4)?;
        let _ = header_end;

        let utf8 = flags & UTF8_FLAG != 0;
        let pool_base = strings_start;
        let mut strings = Vec::with_capacity(string_count);
        for off in offsets {
            let start = pool_base + off;
            if start >= chunk_size {
                return Err(err("axml.stringpool.offset"));
            }
            let region = &b[chunk_start..chunk_start + chunk_size];
            let s = if utf8 {
                let (len, hdr) = read_utf8_len8(region, start)?;
                let from = start + hdr;
                if from + len > region.len() {
                    return Err(err("axml.stringpool.truncated"));
                }
                String::from_utf8_lossy(&region[from..from + len]).into_owned()
            } else {
                let (len_u16, hdr) = read_utf16_len8(region, start)?;
                let from = start + hdr;
                if from + len_u16 * 2 > region.len() {
                    return Err(err("axml.stringpool.truncated"));
                }
                let mut units = Vec::with_capacity(len_u16);
                for i in 0..len_u16 {
                    units.push(u16::from_le_bytes(
                        region[from + i * 2..from + i * 2 + 2].try_into().unwrap(),
                    ));
                }
                String::from_utf16_lossy(&units)
            };
            strings.push(s);
        }
        Ok(Self { strings })
    }
}

struct Chunk<'a> {
    ty: u16,
    #[allow(dead_code)]
    header_size: usize,
    body: &'a [u8],
    /// Absolute offset just past the chunk.
    next: usize,
}

fn next_chunk<'a>(b: &'a [u8], pos: usize) -> Result<Option<Chunk<'a>>> {
    if pos >= b.len() {
        return Ok(None);
    }
    if pos + 8 > b.len() {
        return Err(err("axml.chunk.truncated"));
    }
    let ty = u16::from_le_bytes(b[pos..pos + 2].try_into().unwrap());
    let header_size = u16::from_le_bytes(b[pos + 2..pos + 4].try_into().unwrap()) as usize;
    let size = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
    if size < 8 || pos + size > b.len() || header_size > size {
        return Err(err("axml.chunk.bounds"));
    }
    Ok(Some(Chunk {
        ty,
        header_size,
        body: &b[pos + 8..pos + size],
        next: pos + size,
    }))
}

/// Iterate start-elements of a binary XML document.
pub fn parse_axml(b: &[u8]) -> Result<Vec<Element>> {
    let first = next_chunk(b, 0)?.ok_or_else(|| err("axml.empty"))?;
    if first.ty != RES_XML_TYPE {
        return Err(err("axml.not_xml"));
    }
    // RES_XML_TYPE's size spans the whole document; children follow its header.
    let mut pos = first.header_size.max(8);
    let mut pool = StringPool::default();
    let mut res_map: Vec<u32> = Vec::new();
    let mut elements = Vec::new();
    let mut depth = 0usize;

    while let Some(chunk) = next_chunk(b, pos)? {
        pos = chunk.next;
        match chunk.ty {
            RES_STRING_POOL_TYPE => {
                pool = StringPool::parse(
                    b,
                    pos - chunk.body.len() - 8,
                    chunk.header_size + 8,
                    chunk.body.len() + 8,
                )?;
            }
            RES_XML_RESOURCE_MAP_TYPE => {
                res_map = chunk
                    .body
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_le_bytes(*c))
                    .collect();
            }
            RES_XML_START_ELEMENT_TYPE => {
                let mut r = R::new(chunk.body);
                r.skip(4)?; // line number
                r.skip(4)?; // comment
                let _ns = r.u32()?;
                let name_idx = r.u32()?;
                let name = pool
                    .get(name_idx)
                    .ok_or_else(|| err("axml.name.oob"))?
                    .to_string();
                let attr_start = r.u16()? as usize;
                let attr_size = r.u16()? as usize;
                let attr_count = r.u16()? as usize;
                let _id = r.u16()?;
                let _class = r.u16()?;
                let _style = r.u16()?;

                // attrStart is measured from the start of ResXMLTree_attrExt,
                // which begins right after the 8-byte node header (lineNumber,
                // comment) inside the chunk body.
                let attr_base = 8 + attr_start;
                if attr_size < 20 {
                    return Err(err("axml.attr.size"));
                }
                let mut attrs = Vec::with_capacity(attr_count);
                for i in 0..attr_count {
                    let at = attr_base + i * attr_size;
                    if at + 20 > chunk.body.len() {
                        return Err(err("axml.attr.truncated"));
                    }
                    let mut ar = R::new(&chunk.body[at..at + 20]);
                    let ans = ar.u32()?;
                    let aname = ar.u32()?;
                    let raw = ar.u32()?;
                    ar.skip(2)?; // typedValue size (always 8)
                    ar.u8()?; // res0
                    let data_type = ar.u8()?;
                    let data = ar.u32()?;

                    let attr_name = if aname == 0xffff_ffff {
                        String::new()
                    } else {
                        pool.get(aname)
                            .map(|s| s.to_string())
                            .or_else(|| {
                                // Fall back to the resource map id name, which
                                // AOSP manifests encode for android: attrs.
                                res_map.get(aname as usize).map(|id| format!("@0x{id:08x}"))
                            })
                            .ok_or_else(|| err("axml.attr.name_oob"))?
                    };
                    let ns_uri = if ans == 0xffff_ffff {
                        String::new()
                    } else {
                        pool.get(ans).unwrap_or("").to_string()
                    };
                    let value = decode_value(raw, data_type, data, &pool)?;
                    attrs.push(Attribute {
                        name: attr_name,
                        ns: ns_uri,
                        value,
                    });
                }
                elements.push(Element { name, attrs, depth });
                depth += 1;
            }
            RES_XML_END_ELEMENT_TYPE => depth = depth.saturating_sub(1),
            RES_XML_START_NAMESPACE_TYPE | RES_XML_END_NAMESPACE_TYPE | RES_XML_CDATA_TYPE => {}
            _ => {} // ignore unknown chunk types
        }
    }
    Ok(elements)
}

fn decode_value(raw: u32, data_type: u8, data: u32, pool: &StringPool) -> Result<Value> {
    // The typed value is authoritative for non-string types; rawValue only
    // substitutes when the attribute is (or stays) a string.
    if matches!(data_type, 0 | TYPE_STRING) && raw != 0xffff_ffff {
        if let Some(s) = pool.get(raw) {
            return Ok(Value::Str(s.to_string()));
        }
    }
    Ok(match data_type {
        TYPE_NULL => Value::Null,
        TYPE_STRING => pool
            .get(data)
            .map(|s| Value::Str(s.to_string()))
            .unwrap_or(Value::Null),
        TYPE_INT_DEC => Value::Int(data as i32 as i64),
        TYPE_INT_HEX => Value::Int(data as i64),
        TYPE_INT_BOOLEAN => Value::Bool(data != 0),
        TYPE_REFERENCE => Value::Reference(data),
        TYPE_ATTRIBUTE => Value::Other { data_type, data },
        TYPE_FLOAT => Value::Float(f32::from_bits(data)),
        TYPE_DIMENSION | TYPE_FRACTION => Value::Other { data_type, data },
        _ => Value::Other { data_type, data },
    })
}

/// Information extracted from the binary manifest.
#[derive(Debug, Clone, Default)]
pub struct ManifestInfo {
    /// Package name (e.g. `com.example.app`).
    pub package: String,
    /// `android:versionCode`.
    pub version_code: i64,
    /// `android:versionName`.
    pub version_name: Option<String>,
    /// `uses-sdk/@android:minSdkVersion`.
    pub min_sdk: Option<i64>,
    /// `uses-sdk/@android:targetSdkVersion`.
    pub target_sdk: Option<i64>,
    /// `uses-permission` names.
    pub permissions: Vec<String>,
    /// `uses-feature` names.
    pub features: Vec<String>,
    /// Component class names declared in `<application>`.
    pub activities: Vec<String>,
    /// Declared services.
    pub services: Vec<String>,
    /// Declared receivers.
    pub receivers: Vec<String>,
    /// Declared providers.
    pub providers: Vec<String>,
    /// Provider startup metadata in manifest order.
    pub provider_details: Vec<ProviderEntry>,
    /// Activity name with MAIN action + LAUNCHER category, if any.
    pub launcher_activity: Option<String>,
    /// `split` attribute — `None` marks the base APK (§03 splits).
    pub split: Option<String>,
    /// `configForSplit` — the split this config split applies to.
    pub config_for_split: Option<String>,
    /// `isFeatureSplit` — a dynamic-feature split.
    pub is_feature_split: bool,
    /// `application/@android:name` — the custom Application class.
    pub application_class: Option<String>,
    /// `application/@android:theme` resource id.
    pub theme: Option<u32>,
    /// `activity/@android:theme` resource ids by qualified activity name.
    pub activity_themes: std::collections::HashMap<String, u32>,
    /// `application/@android:icon` resource id — the launcher icon.
    pub icon: Option<u32>,
    /// `application/@android:label` — a string resource reference or a
    /// literal string (PackageItemInfo.loadLabel's app name).
    pub label: Option<Value>,
    /// androidx.startup initializers: `meta-data` names under the
    /// `androidx.startup.InitializationProvider` whose value is
    /// `androidx.startup` (e.g. `androidx.work.WorkManagerInitializer`).
    /// Android runs each Initializer's `create(context)` before
    /// `Application.onCreate`.
    pub startup_initializers: Vec<String>,
    /// Activity intent filters in manifest order. Runtime activity
    /// resolution uses these for implicit intra-app launches such as a
    /// file manager's ACTION_VIEW text editor.
    pub activity_intent_filters: Vec<IntentFilterEntry>,
}

/// One activity `<intent-filter>` declaration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntentFilterEntry {
    /// Qualified activity class name.
    pub activity: String,
    /// Declared action names.
    pub actions: Vec<String>,
    /// Declared category names.
    pub categories: Vec<String>,
    /// Declared data MIME types.
    pub mime_types: Vec<String>,
    /// Declared data schemes.
    pub schemes: Vec<String>,
}

/// Manifest `<provider>` startup metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderEntry {
    /// Qualified provider class name.
    pub name: String,
    /// `android:enabled` (true when absent).
    pub enabled: bool,
    /// `android:process`, when declared.
    pub process: Option<String>,
    /// `android:initOrder` (zero when absent or resource-backed).
    pub init_order: i64,
    /// `android:initOrder` as a resource reference, when used.
    pub init_order_ref: Option<u32>,
}

/// Parse a binary manifest into `ManifestInfo`.
pub fn parse_manifest(b: &[u8]) -> Result<ManifestInfo> {
    let elements = parse_axml(b)?;
    let mut info = ManifestInfo::default();
    // Track per-intent-filter state.
    let mut cur_activity_for_filter: Option<String> = None;
    let mut filter_main = false;
    let mut filter_launcher = false;
    let mut in_filter = false;
    let mut filter_owner: Option<String> = None;
    let mut filter_actions: Vec<String> = Vec::new();
    let mut filter_categories: Vec<String> = Vec::new();
    let mut filter_mime_types: Vec<String> = Vec::new();
    let mut filter_schemes: Vec<String> = Vec::new();
    let mut component_is_activity = false;
    // Name of the provider element currently open, if any — meta-data
    // children of androidx.startup.InitializationProvider declare the
    // app's Initializer classes.
    let mut cur_provider: Option<String> = None;

    // We only have start-elements with depths; reconstruct the tree path by
    // consuming elements in document order, maintaining a stack via depth.
    let mut stack: Vec<String> = Vec::new();
    for el in &elements {
        while stack.len() > el.depth {
            // Pop finished elements; when an intent-filter finishes, evaluate.
            let popped = stack.pop().unwrap();
            if popped == "intent-filter" && in_filter {
                if filter_main && filter_launcher {
                    if let Some(act) = &filter_owner {
                        if info.launcher_activity.is_none() {
                            info.launcher_activity = Some(qualify(&info.package, act));
                        }
                    }
                }
                if let Some(act) = filter_owner.take() {
                    info.activity_intent_filters.push(IntentFilterEntry {
                        activity: act,
                        actions: std::mem::take(&mut filter_actions),
                        categories: std::mem::take(&mut filter_categories),
                        mime_types: std::mem::take(&mut filter_mime_types),
                        schemes: std::mem::take(&mut filter_schemes),
                    });
                } else {
                    filter_actions.clear();
                    filter_categories.clear();
                    filter_mime_types.clear();
                    filter_schemes.clear();
                }
                in_filter = false;
                filter_main = false;
                filter_launcher = false;
            }
            if matches!(
                popped.as_str(),
                "activity" | "activity-alias" | "service" | "receiver"
            ) {
                component_is_activity = false;
            }
            if popped == "provider" {
                cur_provider = None;
            }
        }

        match el.name.as_str() {
            "manifest" => {
                if el.depth == 0 {
                    // package is a plain attribute (no namespace)
                    if let Some(Value::Str(s)) = el
                        .attrs
                        .iter()
                        .find(|a| a.name == "package")
                        .map(|a| &a.value)
                    {
                        info.package = s.clone();
                    }
                    info.version_code = el
                        .attrs
                        .iter()
                        .find(|a| a.name == "versionCode")
                        .and_then(|a| match &a.value {
                            Value::Int(i) => Some(*i),
                            _ => None,
                        })
                        .unwrap_or(0);
                    info.version_name =
                        el.attrs
                            .iter()
                            .find(|a| a.name == "versionName")
                            .and_then(|a| match &a.value {
                                Value::Str(s) => Some(s.clone()),
                                _ => None,
                            });
                    info.split = el
                        .attrs
                        .iter()
                        .find(|a| a.name == "split")
                        .and_then(|a| match &a.value {
                            Value::Str(s) if !s.is_empty() => Some(s.clone()),
                            _ => None,
                        });
                    info.config_for_split = el
                        .attrs
                        .iter()
                        .find(|a| a.name == "configForSplit")
                        .and_then(|a| match &a.value {
                            Value::Str(s) if !s.is_empty() => Some(s.clone()),
                            _ => None,
                        });
                    info.is_feature_split = el
                        .attrs
                        .iter()
                        .find(|a| a.name == "isFeatureSplit")
                        .and_then(|a| match &a.value {
                            Value::Int(i) => Some(*i != 0),
                            Value::Str(s) => Some(s == "true"),
                            _ => None,
                        })
                        .unwrap_or(false);
                }
            }
            "application" => {
                info.application_class = el.android_str("name").map(|n| qualify(&info.package, n));
                info.theme = match el.android("theme") {
                    Some(Value::Reference(r)) => Some(*r),
                    _ => None,
                };
                info.icon = match el.android("icon") {
                    Some(Value::Reference(r)) => Some(*r),
                    _ => None,
                };
                info.label = el.android("label").and_then(|v| match v {
                    Value::Reference(r) => Some(Value::Reference(*r)),
                    Value::Str(s) => Some(Value::Str(s.clone())),
                    _ => None,
                });
            }
            "uses-sdk" => {
                info.min_sdk = el.android_int("minSdkVersion");
                info.target_sdk = el.android_int("targetSdkVersion");
            }
            "uses-permission" | "uses-permission-sdk-23" => {
                if let Some(n) = el.android_str("name") {
                    info.permissions.push(n.to_string());
                }
            }
            "uses-feature" => {
                if let Some(n) = el.android_str("name") {
                    info.features.push(n.to_string());
                }
            }
            "activity" | "activity-alias" => {
                if let Some(n) = el.android_str("name") {
                    let q = qualify(&info.package, n);
                    info.activities.push(q.clone());
                    // An alias launches its targetActivity, not itself.
                    cur_activity_for_filter = Some(if el.name == "activity-alias" {
                        el.android_str("targetActivity")
                            .map(|t| qualify(&info.package, t))
                            .unwrap_or_else(|| q.clone())
                    } else {
                        q.clone()
                    });
                    component_is_activity = true;
                    if let Some(Value::Reference(t)) = el.android("theme") {
                        info.activity_themes.insert(q, *t);
                    }
                }
            }
            "service" => {
                component_is_activity = false;
                if let Some(n) = el.android_str("name") {
                    info.services.push(qualify(&info.package, n));
                }
            }
            "receiver" => {
                component_is_activity = false;
                if let Some(n) = el.android_str("name") {
                    info.receivers.push(qualify(&info.package, n));
                }
            }
            "provider" => {
                if let Some(n) = el.android_str("name") {
                    let q = qualify(&info.package, n);
                    info.providers.push(q.clone());
                    info.provider_details.push(ProviderEntry {
                        name: q.clone(),
                        enabled: !matches!(el.android("enabled"), Some(Value::Bool(false))),
                        process: el.android_str("process").map(str::to_string),
                        init_order: el.android_int("initOrder").unwrap_or(0),
                        init_order_ref: match el.android("initOrder") {
                            Some(Value::Reference(r)) => Some(*r),
                            _ => None,
                        },
                    });
                    cur_provider = Some(q);
                }
            }
            "meta-data"
                if cur_provider.as_deref() == Some("androidx.startup.InitializationProvider")
                    && el.android_str("value") == Some("androidx.startup") =>
            {
                if let Some(n) = el.android_str("name") {
                    info.startup_initializers.push(n.to_string());
                }
            }
            "intent-filter" => {
                in_filter = true;
                filter_main = false;
                filter_launcher = false;
                filter_owner = component_is_activity
                    .then(|| cur_activity_for_filter.clone())
                    .flatten();
                filter_actions.clear();
                filter_categories.clear();
                filter_mime_types.clear();
                filter_schemes.clear();
            }
            "action" if in_filter => {
                if let Some(name) = el.android_str("name") {
                    filter_main |= name == "android.intent.action.MAIN";
                    filter_actions.push(name.to_string());
                }
            }
            "category" if in_filter => {
                if let Some(name) = el.android_str("name") {
                    filter_launcher |= name == "android.intent.category.LAUNCHER";
                    filter_categories.push(name.to_string());
                }
            }
            "data" if in_filter => {
                if let Some(mime) = el.android_str("mimeType") {
                    filter_mime_types.push(mime.to_string());
                }
                if let Some(scheme) = el.android_str("scheme") {
                    filter_schemes.push(scheme.to_string());
                }
            }
            _ => {}
        }
        stack.push(el.name.clone());
    }
    // Close remaining open elements.
    while let Some(popped) = stack.pop() {
        if popped == "intent-filter" && in_filter {
            if filter_main && filter_launcher {
                if let Some(act) = &filter_owner {
                    if info.launcher_activity.is_none() {
                        info.launcher_activity = Some(qualify(&info.package, act));
                    }
                }
            }
            if let Some(act) = filter_owner.take() {
                info.activity_intent_filters.push(IntentFilterEntry {
                    activity: act,
                    actions: std::mem::take(&mut filter_actions),
                    categories: std::mem::take(&mut filter_categories),
                    mime_types: std::mem::take(&mut filter_mime_types),
                    schemes: std::mem::take(&mut filter_schemes),
                });
            }
            in_filter = false;
        }
    }
    if info.package.is_empty() {
        return Err(err("manifest.package.missing"));
    }
    Ok(info)
}

/// Resolve `.Foo` / `Foo` relative names against the package.
fn qualify(package: &str, name: &str) -> String {
    if name.starts_with('.') {
        format!("{package}{name}")
    } else if name.contains('.') {
        name.to_string()
    } else {
        format!("{package}.{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal AXML document with a string pool and one manifest tree.
    // Layout: [XML header][string pool][start manifest][start uses-sdk][end uses-sdk][end manifest]
    fn str_pool_utf8(strings: &[&str]) -> Vec<u8> {
        let n = strings.len();
        let header = 8 + 5 * 4;
        let mut offsets = Vec::new();
        let mut data = Vec::new();
        for s in strings {
            offsets.push(data.len() as u32);
            let b = s.as_bytes();
            assert!(b.len() < 0x80);
            data.push(b.len() as u8); // utf16 len
            data.push(b.len() as u8); // utf8 len
            data.extend(b);
            data.push(0);
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let size = header + n * 4 + data.len();
        let mut out = Vec::new();
        out.extend(RES_STRING_POOL_TYPE.to_le_bytes());
        out.extend((header as u16).to_le_bytes());
        out.extend((size as u32).to_le_bytes());
        out.extend((n as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes()); // styles
        out.extend(UTF8_FLAG.to_le_bytes());
        out.extend(((header + n * 4) as u32).to_le_bytes()); // stringsStart
        out.extend(0u32.to_le_bytes()); // stylesStart
        for o in offsets {
            out.extend(o.to_le_bytes());
        }
        out.extend(data);
        out
    }

    fn start_elem(
        name_idx: u32,
        attrs: &[(u32, u32, u32, u8, u32)], // ns, name, raw, type, data
    ) -> Vec<u8> {
        let attr_count = attrs.len() as u16;
        let attr_size: u16 = 20;
        let ext_header = 4 + 4 + 4 + 4 + 12; // line, comment, ns, name, attrStart/size/count/id/class/style
        let header_size = 8 + ext_header; // chunk header + ext header
        let body_size = ext_header + attrs.len() * attr_size as usize;
        let size = 8 + body_size;
        let mut out = Vec::new();
        out.extend(RES_XML_START_ELEMENT_TYPE.to_le_bytes());
        out.extend((header_size as u16).to_le_bytes());
        out.extend((size as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes()); // line
        out.extend(0u32.to_le_bytes()); // comment
        out.extend(0xffff_ffffu32.to_le_bytes()); // ns
        out.extend(name_idx.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // attrStart (from ext header start)
        out.extend(attr_size.to_le_bytes());
        out.extend(attr_count.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        for (ns, name, raw, ty, data) in attrs {
            out.extend(ns.to_le_bytes());
            out.extend(name.to_le_bytes());
            out.extend(raw.to_le_bytes());
            out.extend(8u16.to_le_bytes());
            out.push(0);
            out.push(*ty);
            out.extend(data.to_le_bytes());
        }
        out
    }

    fn end_elem(name_idx: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(RES_XML_END_ELEMENT_TYPE.to_le_bytes());
        out.extend(24u16.to_le_bytes());
        out.extend(24u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(0xffff_ffffu32.to_le_bytes());
        out.extend(name_idx.to_le_bytes());
        out
    }

    #[test]
    fn parses_minimal_manifest() {
        // strings: 0=manifest 1=uses-sdk 2=package 3=com.test.app
        // 4=minSdkVersion 5=targetSdkVersion
        let strings = [
            "manifest",
            "uses-sdk",
            "package",
            "com.test.app",
            "minSdkVersion",
            "targetSdkVersion",
            "versionCode",
        ];
        let mut doc = Vec::new();
        doc.extend(RES_XML_TYPE.to_le_bytes());
        doc.extend(8u16.to_le_bytes());
        doc.extend(8u32.to_le_bytes());
        doc.extend(str_pool_utf8(&strings));
        // <manifest package="com.test.app" versionCode=7>
        doc.extend(start_elem(
            0,
            &[
                (0xffff_ffff, 2, 3, TYPE_STRING, 3),
                (0xffff_ffff, 6, 0xffff_ffff, TYPE_INT_DEC, 7),
            ],
        ));
        // <uses-sdk minSdkVersion=24 targetSdkVersion=35/>
        doc.extend(start_elem(
            1,
            &[
                (0xffff_ffff, 4, 0xffff_ffff, TYPE_INT_DEC, 24),
                (0xffff_ffff, 5, 0xffff_ffff, TYPE_INT_DEC, 35),
            ],
        ));
        doc.extend(end_elem(1));
        doc.extend(end_elem(0));

        let info = parse_manifest(&doc).unwrap();
        assert_eq!(info.package, "com.test.app");
        assert_eq!(info.version_code, 7);
        assert_eq!(info.min_sdk, Some(24));
        assert_eq!(info.target_sdk, Some(35));
    }

    #[test]
    fn parses_activity_intent_filter_data() {
        let strings = [
            "manifest",
            "package",
            "com.test.app",
            "application",
            "activity",
            "name",
            ".ReadTextActivity",
            "intent-filter",
            "action",
            "android.intent.action.VIEW",
            "category",
            "android.intent.category.DEFAULT",
            "data",
            "mimeType",
            "text/*",
            "scheme",
            "content",
        ];
        let mut doc = Vec::new();
        doc.extend(RES_XML_TYPE.to_le_bytes());
        doc.extend(8u16.to_le_bytes());
        doc.extend(8u32.to_le_bytes());
        doc.extend(str_pool_utf8(&strings));
        doc.extend(start_elem(0, &[(0xffff_ffff, 1, 2, TYPE_STRING, 2)]));
        doc.extend(start_elem(3, &[]));
        doc.extend(start_elem(4, &[(0xffff_ffff, 5, 6, TYPE_STRING, 6)]));
        doc.extend(start_elem(7, &[]));
        doc.extend(start_elem(8, &[(0xffff_ffff, 5, 9, TYPE_STRING, 9)]));
        doc.extend(end_elem(8));
        doc.extend(start_elem(10, &[(0xffff_ffff, 5, 11, TYPE_STRING, 11)]));
        doc.extend(end_elem(10));
        doc.extend(start_elem(
            12,
            &[
                (0xffff_ffff, 13, 14, TYPE_STRING, 14),
                (0xffff_ffff, 15, 16, TYPE_STRING, 16),
            ],
        ));
        doc.extend(end_elem(12));
        doc.extend(end_elem(7));
        doc.extend(end_elem(4));
        doc.extend(end_elem(3));
        doc.extend(end_elem(0));

        let info = parse_manifest(&doc).unwrap();
        assert_eq!(info.activity_intent_filters.len(), 1);
        let filter = &info.activity_intent_filters[0];
        assert_eq!(filter.activity, "com.test.app.ReadTextActivity");
        assert_eq!(filter.actions, ["android.intent.action.VIEW"]);
        assert_eq!(filter.categories, ["android.intent.category.DEFAULT"]);
        assert_eq!(filter.mime_types, ["text/*"]);
        assert_eq!(filter.schemes, ["content"]);
    }
}
