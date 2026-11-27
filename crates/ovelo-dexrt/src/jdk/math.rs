//! Minimal `java.math` numerics. BigDecimal is represented as an f64 —
//! enough for calculator-style arithmetic and formatting, while keeping
//! Java's value-object semantics rather than identity-only host stubs.

use crate::value::{HostState, Obj, V};
use crate::{Rt, Vm};

use super::{host, jdynstr, sv};

const ROUND_UP: i32 = 0;
const ROUND_DOWN: i32 = 1;
const ROUND_CEILING: i32 = 2;
const ROUND_FLOOR: i32 = 3;
const ROUND_HALF_UP: i32 = 4;
const ROUND_HALF_DOWN: i32 = 5;
const ROUND_HALF_EVEN: i32 = 6;

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
    sv(vm, v)
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

fn rounding_mode(vm: &Vm, v: V) -> i32 {
    match v {
        V::I32(i) => i,
        V::I64(i) => i as i32,
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::Slot(s),
                ..
            }) => i32v(*s),
            _ => ROUND_HALF_UP,
        },
        _ => ROUND_HALF_UP,
    }
}

fn round_unit(x: f64, mode: i32) -> f64 {
    match mode {
        ROUND_UP => x.signum() * x.abs().ceil(),
        ROUND_DOWN => x.trunc(),
        ROUND_CEILING => x.ceil(),
        ROUND_FLOOR => x.floor(),
        ROUND_HALF_DOWN => {
            if (x.fract().abs() - 0.5).abs() < 1e-12 {
                x.trunc()
            } else {
                x.round()
            }
        }
        ROUND_HALF_EVEN => {
            let floor = x.floor();
            let diff = x - floor;
            if diff < 0.5 {
                floor
            } else if diff > 0.5 {
                floor + 1.0
            } else if (floor / 2.0).fract() == 0.0 {
                floor
            } else {
                floor + 1.0
            }
        }
        _ => x.round(),
    }
}

fn round_scale_mode(f: f64, scale: i32, mode: i32) -> f64 {
    let scale = scale.clamp(-300, 300);
    let p = 10f64.powi(scale.abs());
    let scaled = if scale >= 0 { f * p } else { f / p };
    let rounded = round_unit(scaled, mode);
    if scale >= 0 {
        rounded / p
    } else {
        rounded * p
    }
}

fn mc_parts(vm: &Vm, v: V) -> Option<(i32, i32)> {
    let V::Ref(h) = v else { return None };
    match vm.heap.get(h) {
        Some(Obj::Host {
            desc,
            state: HostState::List(parts),
            ..
        }) if desc == "Ljava/math/MathContext;" => Some((
            parts.first().map(|v| i32v(*v)).unwrap_or(0),
            parts.get(1).map(|v| i32v(*v)).unwrap_or(ROUND_HALF_UP),
        )),
        _ => None,
    }
}

fn round_precision(f: f64, precision: i32, mode: i32) -> f64 {
    if precision <= 0 || f == 0.0 || !f.is_finite() {
        return f;
    }
    let exponent = f.abs().log10().floor() as i32;
    let scale = precision - 1 - exponent;
    round_scale_mode(f, scale, mode)
}

fn apply_mc(vm: &Vm, args: &[V], f: f64) -> f64 {
    for a in args.iter().skip(1) {
        if let Some((precision, mode)) = mc_parts(vm, *a) {
            return round_precision(f, precision, mode);
        }
    }
    f
}

fn rounding_obj(vm: &mut Vm, mode: i32) -> V {
    host(
        vm,
        "Ljava/math/RoundingMode;",
        HostState::Slot(V::I32(mode)),
    )
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
                if let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) {
                    let precision = args.get(1).map(|v| i32v(*v)).unwrap_or(0);
                    let rounding = args
                        .get(2)
                        .map(|v| rounding_mode(vm, *v))
                        .unwrap_or(ROUND_HALF_UP);
                    if let Some(Obj::Host { state, .. }) = vm.heap.get_mut(h) {
                        *state = HostState::List(vec![V::I32(precision), V::I32(rounding)]);
                    }
                }
                Some(None)
            }
            "getPrecision" => Some(Some(V::I32(
                mc_parts(vm, args.first().copied().unwrap_or(V::Ref(0)))
                    .map(|(p, _)| p)
                    .unwrap_or(0),
            ))),
            "getRoundingMode" => {
                let mode = mc_parts(vm, args.first().copied().unwrap_or(V::Ref(0)))
                    .map(|(_, r)| r)
                    .unwrap_or(ROUND_HALF_UP);
                Some(Some(rounding_obj(vm, mode)))
            }
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
                set_bd(vm, h, apply_mc(vm, args, f));
            }
            Some(None)
        }
        "valueOf" => {
            let unscaled = num(vm, args.first().copied().unwrap_or(V::I32(0)));
            let scale = args.get(1).map(|v| i32v(*v)).unwrap_or(0);
            Some(Some(new_bd(vm, unscaled / 10f64.powi(scale))))
        }
        "add" => Some(Some(new_bd(vm, apply_mc(vm, args, tf + num(vm, args[1]))))),
        "subtract" => Some(Some(new_bd(vm, apply_mc(vm, args, tf - num(vm, args[1]))))),
        "multiply" => Some(Some(new_bd(vm, apply_mc(vm, args, tf * num(vm, args[1]))))),
        "divide" => {
            let d = num(vm, args[1]);
            if d == 0.0 {
                return Err(vm.throw_builtin("Ljava/lang/ArithmeticException;", "Division by zero"));
            }
            let raw = tf / d;
            let rounded = if args.len() >= 4 {
                round_scale_mode(raw, i32v(args[2]), rounding_mode(vm, args[3]))
            } else {
                apply_mc(vm, args, raw)
            };
            Some(Some(new_bd(vm, rounded)))
        }
        "remainder" => {
            let d = num(vm, args[1]);
            if d == 0.0 {
                return Err(vm.throw_builtin("Ljava/lang/ArithmeticException;", "Division by zero"));
            }
            Some(Some(new_bd(vm, apply_mc(vm, args, tf % d))))
        }
        "divideToIntegralValue" => {
            let d = num(vm, args[1]);
            if d == 0.0 {
                return Err(vm.throw_builtin("Ljava/lang/ArithmeticException;", "Division by zero"));
            }
            Some(Some(new_bd(vm, apply_mc(vm, args, (tf / d).trunc()))))
        }
        "pow" => Some(Some(new_bd(
            vm,
            apply_mc(
                vm,
                args,
                tf.powi(i32v(args.get(1).copied().unwrap_or(V::I32(0)))),
            ),
        ))),
        "sqrt" => Some(Some(new_bd(vm, apply_mc(vm, args, tf.sqrt())))),
        "negate" => Some(Some(new_bd(vm, -tf))),
        "abs" => Some(Some(new_bd(vm, tf.abs()))),
        "plus" => Some(Some(new_bd(vm, apply_mc(vm, args, tf)))),
        "round" => Some(Some(new_bd(vm, apply_mc(vm, args, tf)))),
        "stripTrailingZeros" => Some(Some(new_bd(vm, tf))),
        "setScale" => {
            let scale = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            let mode = args
                .get(2)
                .map(|v| rounding_mode(vm, *v))
                .unwrap_or(ROUND_HALF_UP);
            Some(Some(new_bd(vm, round_scale_mode(tf, scale, mode))))
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
        "equals" => {
            let other = args.get(1).copied().unwrap_or(V::Ref(0));
            let eq = match other {
                V::Ref(0) => false,
                V::Ref(h) => bd_of(vm, h).is_some_and(|f| f == tf),
                _ => false,
            };
            Some(Some(V::I32(eq as i32)))
        }
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
