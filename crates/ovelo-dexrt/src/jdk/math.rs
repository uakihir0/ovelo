//! Minimal `java.math` numerics. BigDecimal is represented as an f64 —
//! enough for calculator-style arithmetic and formatting, while keeping
//! Java's value-object semantics rather than identity-only host stubs.

use crate::value::{HostState, Obj, V};
use crate::{Rt, Vm};

use super::{host, jdynstr};

pub(crate) fn bd_of(vm: &Vm, h: u32) -> Option<f64> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            desc,
            state: HostState::Slot(V::F64(f)),
            ..
        }) if desc == "Ljava/math/BigDecimal;" => Some(*f),
        Some(Obj::Host {
            desc,
            state: HostState::Slot(V::I32(i)),
            ..
        }) if desc == "Ljava/math/BigDecimal;" => Some(*i as f64),
        Some(Obj::Host {
            desc,
            state: HostState::Slot(V::I64(i)),
            ..
        }) if desc == "Ljava/math/BigDecimal;" => Some(*i as f64),
        _ => None,
    }
}

fn set_bd(vm: &mut Vm, h: u32, f: f64) {
    if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(h) {
        *state = HostState::Slot(V::F64(f));
    }
}

fn new_bd(vm: &mut Vm, f: f64) -> V {
    host(vm, "Ljava/math/BigDecimal;", HostState::Slot(V::F64(f)))
}

fn value_string(vm: &Vm, v: V) -> Option<String> {
    let V::Ref(h) = v else { return None };
    vm.heap.str_val(h).map(str::to_owned)
}

fn num(vm: &Vm, v: V) -> f64 {
    match v {
        V::I32(i) => i as f64,
        V::I64(i) => i as f64,
        V::F32(f) => f as f64,
        V::F64(f) => f,
        V::Ref(h) => bd_of(vm, h)
            .or_else(|| value_string(vm, v).and_then(|s| s.trim().parse().ok()))
            .unwrap_or(0.0),
    }
}

pub(crate) fn fmt_bd(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() {
            "-Infinity"
        } else {
            "Infinity"
        }
        .into();
    }
    if f == 0.0 {
        return "0".into();
    }
    if f.fract() == 0.0 && f.abs() <= 9.0e15 {
        return format!("{}", f as i64);
    }
    let mut s = format!("{f:.15}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn round_scale(f: f64, scale: i32) -> f64 {
    let p = 10f64.powi(scale.clamp(-300, 300));
    if scale >= 0 {
        (f * p).round() / p
    } else {
        (f / p).round() * p
    }
}

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    _sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    if class == "Ljava/math/MathContext;" {
        return Ok(match name {
            "<init>" => {
                if let (V::Ref(h), Some(p)) = (
                    args.first().copied().unwrap_or(V::Ref(0)),
                    args.get(1).copied(),
                ) {
                    if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(h) {
                        *state = HostState::Slot(p);
                    }
                }
                Some(None)
            }
            "getPrecision" => Some(Some(match args.first().copied().unwrap_or(V::Ref(0)) {
                V::Ref(h) => match vm.heap.get(h) {
                    Some(Obj::Host {
                        state: HostState::Slot(v),
                        ..
                    }) => *v,
                    _ => V::I32(0),
                },
                _ => V::I32(0),
            })),
            "toString" => Some(Some(jdynstr(vm, "MathContext"))),
            _ => None,
        });
    }
    if class != "Ljava/math/BigDecimal;" {
        return Ok(None);
    }
    let this = args.first().copied().unwrap_or(V::Ref(0));
    if crate::dbg_flag("OVELO_MATHDBG") {
        eprintln!("[mathdbg] {name} argc={} args={args:?}", args.len());
    }
    let tf = match this {
        V::Ref(h) => bd_of(vm, h).unwrap_or(0.0),
        _ => 0.0,
    };
    Ok(match name {
        "<init>" => {
            if let V::Ref(h) = this {
                let f = args.get(1).copied().map(|v| num(vm, v)).unwrap_or(0.0);
                set_bd(vm, h, f);
            }
            Some(None)
        }
        "valueOf" => Some(Some(new_bd(
            vm,
            num(vm, args.first().copied().unwrap_or(V::I32(0))),
        ))),
        "add" => Some(Some(new_bd(vm, tf + num(vm, args[1])))),
        "subtract" => Some(Some(new_bd(vm, tf - num(vm, args[1])))),
        "multiply" => Some(Some(new_bd(vm, tf * num(vm, args[1])))),
        "divide" => Some(Some(new_bd(vm, tf / num(vm, args[1])))),
        "remainder" => Some(Some(new_bd(vm, tf % num(vm, args[1])))),
        "divideToIntegralValue" => Some(Some(new_bd(vm, (tf / num(vm, args[1])).trunc()))),
        "pow" => Some(Some(new_bd(
            vm,
            tf.powi(i32v(args.get(1).copied().unwrap_or(V::I32(0)))),
        ))),
        "sqrt" => Some(Some(new_bd(vm, tf.sqrt()))),
        "negate" => Some(Some(new_bd(vm, -tf))),
        "abs" => Some(Some(new_bd(vm, tf.abs()))),
        "plus" => Some(Some(new_bd(vm, tf))),
        "round" | "stripTrailingZeros" => Some(Some(new_bd(vm, tf))),
        "setScale" => {
            let scale = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            Some(Some(new_bd(vm, round_scale(tf, scale))))
        }
        "scale" => {
            let s = fmt_bd(tf);
            let n = s.rsplit_once('.').map(|(_, f)| f.len() as i32).unwrap_or(0);
            Some(Some(V::I32(n)))
        }
        "precision" => {
            let s = fmt_bd(tf.abs());
            let n = s.chars().filter(|c| c.is_ascii_digit()).count() as i32;
            Some(Some(V::I32(n.max(1))))
        }
        "signum" => Some(Some(V::I32(if tf > 0.0 {
            1
        } else if tf < 0.0 {
            -1
        } else {
            0
        }))),
        "movePointLeft" => {
            let n = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            Some(Some(new_bd(vm, tf / 10f64.powi(n))))
        }
        "movePointRight" => {
            let n = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            Some(Some(new_bd(vm, tf * 10f64.powi(n))))
        }
        "max" => Some(Some(new_bd(vm, tf.max(num(vm, args[1]))))),
        "min" => Some(Some(new_bd(vm, tf.min(num(vm, args[1]))))),
        "compareTo" => {
            let other = num(vm, args.get(1).copied().unwrap_or(V::I32(0)));
            Some(Some(V::I32(if tf > other {
                1
            } else if tf < other {
                -1
            } else {
                0
            })))
        }
        "equals" => Some(Some(V::I32(
            (tf == num(vm, args.get(1).copied().unwrap_or(V::I32(0)))) as i32,
        ))),
        "hashCode" => Some(Some(V::I32((tf.to_bits() ^ (tf.to_bits() >> 32)) as i32))),
        "toString" | "toPlainString" | "toEngineeringString" => {
            Some(Some(jdynstr(vm, &fmt_bd(tf))))
        }
        "doubleValue" => Some(Some(V::F64(tf))),
        "floatValue" => Some(Some(V::F32(tf as f32))),
        "longValue" | "longValueExact" => Some(Some(V::I64(tf as i64))),
        "intValue" | "intValueExact" => Some(Some(V::I32(tf as i32))),
        "shortValue" | "shortValueExact" => Some(Some(V::I32(tf as i16 as i32))),
        "byteValue" | "byteValueExact" => Some(Some(V::I32(tf as i8 as i32))),
        _ => None,
    })
}

fn i32v(v: V) -> i32 {
    match v {
        V::I32(i) => i,
        V::I64(i) => i as i32,
        V::F32(f) => f as i32,
        V::F64(f) => f as i32,
        _ => 0,
    }
}
