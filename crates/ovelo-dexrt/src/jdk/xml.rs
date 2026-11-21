//! `android.util.Xml` and `XmlResourceParser`: a pull parser over the
//! installed APK's binary XML (`res.rs`), so inflaters
//! (`PreferenceInflater`, menu/nav inflaters) walk real `res/xml/`
//! documents instead of null parsers.

use super::*;
use crate::value::{xml_events, HostState, XmlEvent};
use ovelo_apk::manifest::{Element, Value};
use std::rc::Rc;

// XmlPullParser event codes (org.xmlpull.v1).
const START_DOCUMENT: i32 = 0;
const END_DOCUMENT: i32 = 1;
const START_TAG: i32 = 2;
const END_TAG: i32 = 3;

const PARSER: &str = "Landroid/content/res/XmlResourceParser;";

/// `Resources.getXml(resId)` → a pull parser over the resource's binary
/// XML; `null` when the id isn't a file-backed XML resource.
pub(crate) fn parser_of(vm: &mut Vm, res_id: u32) -> V {
    let Some(elements) = vm.apk_res.as_ref().and_then(|r| r.document(res_id)) else {
        return V::Ref(0);
    };
    let elements = Rc::new(elements);
    let events = xml_events(&elements);
    V::Ref(vm.alloc_host(
        PARSER,
        HostState::Xml {
            elements,
            events,
            pos: 0,
        },
    ))
}

fn code_of(e: XmlEvent) -> i32 {
    match e {
        XmlEvent::StartDocument => START_DOCUMENT,
        XmlEvent::Start(_) => START_TAG,
        XmlEvent::End(_) => END_TAG,
        XmlEvent::EndDocument => END_DOCUMENT,
    }
}

/// Current event + shared element list of a parser handle.
fn cur(vm: &Vm, h: u32) -> Option<(Rc<Vec<Element>>, XmlEvent)> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state:
                HostState::Xml {
                    elements,
                    events,
                    pos,
                },
            ..
        }) => Some((elements.clone(), events.get(*pos).copied()?)),
        _ => None,
    }
}

fn cur_el(els: &[Element], e: XmlEvent) -> Option<&Element> {
    match e {
        XmlEvent::Start(i) | XmlEvent::End(i) => els.get(i),
        _ => None,
    }
}

fn advance(vm: &mut Vm, h: u32) -> Option<XmlEvent> {
    match vm.heap.get_mut(h) {
        Some(Obj::Host {
            state: HostState::Xml { events, pos, .. },
            ..
        }) => {
            if *pos + 1 < events.len() {
                *pos += 1;
            }
            events.get(*pos).copied()
        }
        _ => None,
    }
}

/// String form of a binary-XML attribute value for
/// `getAttributeValue` — references stringify as `@<dec id>` like the
/// framework's TypedValue coercion.
fn attr_s(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.into()),
        Value::Float(f) => Some(fmt_f64(*f as f64)),
        Value::Reference(r) => Some(format!("@{r}")),
        Value::Other { .. } | Value::Null => None,
    }
}

/// Value of the attribute `name` on the element the cursor sits on,
/// converting only that one value — used by `ta_lookup` so a styleable
/// read doesn't materialize the whole attribute list per index.
pub(crate) fn cur_attr_value(vm: &mut Vm, h: u32, name: &str) -> Option<V> {
    let (els, ev) = cur(vm, h)?;
    let el = cur_el(&els, ev)?;
    el.attrs
        .iter()
        .find(|a| a.name == name)
        .map(|a| view::attr_v(vm, &a.value))
}

pub(crate) fn pull_call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    let _ = class;
    let h = match args.first() {
        Some(&V::Ref(h)) => h,
        _ => 0,
    };
    Ok(match name {
        "getEventType" => Some(Some(V::I32(
            cur(vm, h).map(|(_, e)| code_of(e)).unwrap_or(END_DOCUMENT),
        ))),
        "next" | "nextToken" => Some(Some(V::I32(
            advance(vm, h).map(code_of).unwrap_or(END_DOCUMENT),
        ))),
        "nextTag" => {
            let mut e = advance(vm, h);
            while matches!(e, Some(XmlEvent::StartDocument)) {
                e = advance(vm, h);
            }
            Some(Some(V::I32(e.map(code_of).unwrap_or(END_DOCUMENT))))
        }
        "getName" => {
            let t = cur(vm, h).and_then(|(els, e)| cur_el(&els, e).map(|el| el.name.clone()));
            Some(Some(t.map(|t| jstr(vm, &t)).unwrap_or(V::Ref(0))))
        }
        "getNamespace" | "getText" | "getInputEncoding" | "getPrefix" | "getAttributePrefix"
        | "getFeature" | "getIdAttribute" | "getClassAttribute" => {
            let _ = args;
            Some(Some(V::Ref(0)))
        }
        "getDepth" => Some(Some(V::I32(
            cur(vm, h)
                .and_then(|(els, e)| cur_el(&els, e).map(|el| el.depth as i32 + 1))
                .unwrap_or(0),
        ))),
        "getLineNumber" | "getColumnNumber" => Some(Some(V::I32(-1))),
        "getPositionDescription" => Some(Some(jstr(vm, ""))),
        "getAttributeCount" => Some(Some(V::I32(
            cur(vm, h)
                .and_then(|(els, e)| {
                    if let XmlEvent::Start(i) = e {
                        els.get(i).map(|el| el.attrs.len() as i32)
                    } else {
                        None
                    }
                })
                .unwrap_or(-1),
        ))),
        "getAttributeName" | "getAttributeNamespace" => {
            let i = i32v(args.get(1).copied().unwrap_or(V::I32(0))) as usize;
            let t = cur(vm, h).and_then(|(els, e)| {
                cur_el(&els, e).and_then(|el| {
                    el.attrs.get(i).map(|a| {
                        if name == "getAttributeName" {
                            a.name.clone()
                        } else {
                            a.ns.clone()
                        }
                    })
                })
            });
            Some(Some(t.map(|t| jstr(vm, &t)).unwrap_or(V::Ref(0))))
        }
        "getAttributeValue" => {
            // Two shapes: (int) and (ns, name).
            let t = cur(vm, h).and_then(|(els, e)| {
                cur_el(&els, e).and_then(|el| match args.get(1).copied() {
                    Some(V::I32(i)) => el.attrs.get(i as usize).and_then(|a| attr_s(&a.value)),
                    _ => {
                        let want = args.get(2).and_then(|a| sv(vm, *a));
                        el.attrs
                            .iter()
                            .find(|a| Some(a.name.as_str()) == want.as_deref())
                            .and_then(|a| attr_s(&a.value))
                    }
                })
            });
            Some(Some(t.map(|t| jstr(vm, &t)).unwrap_or(V::Ref(0))))
        }
        "getAttributeResourceValue" | "getAttributeUnsignedIntValue" | "getAttributeIntValue" => {
            let i = args.get(1).map(|v| i64v(*v)).unwrap_or(0);
            let found = cur(vm, h).and_then(|(els, e)| {
                cur_el(&els, e)
                    .and_then(|el| el.attrs.get(i.max(0) as usize))
                    .map(|a| match &a.value {
                        Value::Reference(r) => *r as i64,
                        Value::Int(v) => *v,
                        Value::Bool(b) => *b as i64,
                        _ => 0,
                    })
            });
            // Signature: index args + a default last.
            Some(Some(found.map(|v| V::I32(v as i32)).unwrap_or_else(|| {
                args.last().copied().unwrap_or(V::I32(0))
            })))
        }
        "getAttributeBooleanValue" => {
            let i = args.get(1).map(|v| i64v(*v)).unwrap_or(0);
            let found = cur(vm, h).and_then(|(els, e)| {
                cur_el(&els, e)
                    .and_then(|el| el.attrs.get(i.max(0) as usize))
                    .map(|a| match &a.value {
                        Value::Bool(b) => *b as i32,
                        Value::Int(v) => (*v != 0) as i32,
                        _ => 0,
                    })
            });
            Some(Some(found.map(V::I32).unwrap_or_else(|| {
                args.last().copied().unwrap_or(V::I32(0))
            })))
        }
        "getAttributeFloatValue" => {
            let i = args.get(1).map(|v| i64v(*v)).unwrap_or(0);
            let found = cur(vm, h).and_then(|(els, e)| {
                cur_el(&els, e)
                    .and_then(|el| el.attrs.get(i.max(0) as usize))
                    .map(|a| match &a.value {
                        Value::Float(f) => *f as f64,
                        Value::Int(v) => *v as f64,
                        _ => 0.0,
                    })
            });
            Some(Some(found.map(V::F64).unwrap_or_else(|| {
                args.last().copied().unwrap_or(V::F32(0.0))
            })))
        }
        "getAttributeNameResource"
        | "getAttributeListValue"
        | "getIdAttributeResourceValue"
        | "getStyleAttribute" => Some(Some(args.last().copied().unwrap_or(V::I32(0)))),
        "isEmptyElementTag" => {
            let empty = cur(vm, h)
                .and_then(|(els, e)| {
                    if let XmlEvent::Start(i) = e {
                        Some(match els.get(i + 1) {
                            Some(next) => next.depth <= els[i].depth,
                            None => true,
                        })
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            Some(Some(V::I32(empty as i32)))
        }
        "getAttributeType" => Some(Some(jstr(vm, "CDATA"))),
        "getProperty" => Some(Some(args.get(1).copied().unwrap_or(V::Ref(0)))),
        "isWhitespace" => Some(Some(V::I32(0))),
        "setInput"
        | "setFeature"
        | "setProperty"
        | "defineEntityReplacementText"
        | "close"
        | "require" => Some(None),
        _ => Some(match ret(sig) {
            "V" => None,
            _ => Some(stub_ret(vm, sig)),
        }),
    })
}
