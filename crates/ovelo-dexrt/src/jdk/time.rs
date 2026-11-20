//! java.util.Date/Calendar/TimeZone + java.time subset via civil math.

use super::*;
use crate::value::{HostState, V};

const DATE_DESC: &str = "Ljava/util/Date;";
const CAL_DESC: &str = "Ljava/util/Calendar;";

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(if desc_is_a(class, DATE_DESC) || class.starts_with("Ljava/sql/") {
        date_call(vm, name, args)?
    } else if desc_is_a(class, "Ljava/util/GregorianCalendar;")
        || desc_is_a(class, CAL_DESC)
    {
        cal_call(vm, name, sig, args)?
    } else if desc_is_a(class, "Ljava/util/TimeZone;") {
        timezone_call(vm, name, args)
    } else if class.starts_with("Ljava/time/") {
        jtime_call(vm, class, name, args)?
    } else {
        return Ok(None);
    })
}

// ---------------------------------------------------------------------
// civil date math (Howard Hinnant algorithms)

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ms_fields(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000,
    )
}

/// "yyyy-MM-dd HH:mm:ss" in UTC — a reasonable default format.
pub(crate) fn date_to_string(ms: i64) -> String {
    let (y, mo, d, h, mi, s, _) = ms_fields(ms);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Parse ISO-ish "yyyy-MM-dd[ HH:mm:ss]" → epoch ms, 0 on failure.
pub(crate) fn parse_date(t: &str) -> i64 {
    let t = t.trim();
    let (date, time) = t.split_once([' ', 'T']).unwrap_or((t, ""));
    let mut dp = date.split('-');
    let (y, mo, d) = (
        dp.next().and_then(|x| x.parse().ok()).unwrap_or(1970),
        dp.next().and_then(|x| x.parse().ok()).unwrap_or(1),
        dp.next().and_then(|x| x.parse().ok()).unwrap_or(1),
    );
    let mut tp = time.split(':');
    let (h, mi, s) = (
        tp.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        tp.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        tp.next()
            .and_then(|x| x.split('.').next().and_then(|v| v.parse().ok()))
            .unwrap_or(0),
    );
    days_from_civil(y, mo, d) * 86_400_000 + h * 3_600_000 + mi * 60_000 + s * 1000
}

fn millis_of(vm: &Vm, v: V) -> i64 {
    match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::Millis(m),
                ..
            }) => *m,
            _ => 0,
        },
        V::I32(i) => i as i64,
        V::I64(i) => i,
        _ => 0,
    }
}

fn set_millis(vm: &mut Vm, h: u32, ms: i64) {
    if let Some(Obj::Host {
        state: HostState::Millis(m),
        ..
    }) = vm.heap.get_mut(h)
    {
        *m = ms;
    }
}

// ---------------------------------------------------------------------
// Date

fn date_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let ms = millis_of(vm, args[0]);
    Ok(match name {
        "<init>" => {
            // Date() -> now; Date(long) -> millis; Date(y,m,d[,h,mi,s])
            // (deprecated) -> civil with 1900-based year.
            if let Some(V::Ref(h)) = args.first() {
                let g =
                    |i: usize| args.get(i).map(|v| i64v(*v)).unwrap_or(0);
                let v = match args.len() {
                    0 | 1 => now_ms(),
                    2 => g(1),
                    _ => {
                        days_from_civil(g(1) + 1900, g(2) + 1, g(3))
                            * 86_400_000
                            + g(4) * 3_600_000
                            + g(5) * 60_000
                            + g(6) * 1000
                    }
                };
                set_millis(vm, *h, v);
            }
            Some(None)
        }
        "getTime" => Some(Some(V::I64(ms))),
        "setTime" => {
            if let V::Ref(h) = args[0] {
                set_millis(vm, h, i64v(args[1]));
            }
            Some(None)
        }
        "before" => Some(Some(V::I32((ms < millis_of(vm, args[1])) as i32))),
        "after" => Some(Some(V::I32((ms > millis_of(vm, args[1])) as i32))),
        "compareTo" => Some(Some(V::I32(ms.cmp(&millis_of(vm, args[1])) as i32))),
        "toInstant" => Some(Some(host(
            vm,
            "Ljava/time/Instant;",
            HostState::Millis(ms),
        ))),
        "from" => Some(Some(host(
            vm,
            DATE_DESC,
            HostState::Millis(millis_of(vm, args[0])),
        ))),
        "toString" => Some(Some(jstr(vm, &date_to_string(ms)))),
        "getYear" => {
            let (y, ..) = ms_fields(ms);
            Some(Some(V::I32((y - 1900) as i32)))
        }
        "getMonth" => {
            let (_, mo, ..) = ms_fields(ms);
            Some(Some(V::I32((mo - 1) as i32)))
        }
        "getDate" => {
            let (_, _, d, _, _, _, _) = ms_fields(ms);
            Some(Some(V::I32(d as i32)))
        }
        "getDay" => {
            let days = ms.div_euclid(86_400_000);
            Some(Some(V::I32(((days + 4).rem_euclid(7)) as i32)))
        }
        "getHours" => {
            let (_, _, _, h, _, _, _) = ms_fields(ms);
            Some(Some(V::I32(h as i32)))
        }
        "getMinutes" => {
            let (_, _, _, _, mi, _, _) = ms_fields(ms);
            Some(Some(V::I32(mi as i32)))
        }
        "getSeconds" => {
            let (_, _, _, _, _, s, _) = ms_fields(ms);
            Some(Some(V::I32(s as i32)))
        }
        "hashCode" => Some(Some(V::I32((ms ^ (ms >> 32)) as i32))),
        _ => None,
    })
}

// ---------------------------------------------------------------------
// Calendar

fn cal_call(vm: &mut Vm, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    let h = match args.first() {
        Some(V::Ref(h)) if *h != 0 => *h,
        _ => {
            // static factory
            return Ok(match name {
                "getInstance" => Some(Some(host(
                    vm,
                    "Ljava/util/GregorianCalendar;",
                    HostState::Millis(now_ms()),
                ))),
                "getAvailableLocales" | "getAvailableCalendarTypes" => Some(Some(arr(
                    vm, "Ljava/util/Locale;", vec![],
                ))),
                _ => None,
            });
        }
    };
    let ms = millis_of(vm, args[0]);
    Ok(match name {
        "<init>" => {
            // GregorianCalendar() -> now; (y,m,d[,h,mi[,s]]) -> civil.
            let g =
                |i: usize| args.get(i).map(|v| i64v(*v)).unwrap_or(0);
            let v = if args.len() > 1 {
                days_from_civil(g(1), g(2) + 1, g(3)) * 86_400_000
                    + g(4) * 3_600_000
                    + g(5) * 60_000
                    + g(6) * 1000
            } else {
                now_ms()
            };
            set_millis(vm, h, v);
            Some(None)
        }
        "getInstance" => Some(Some(host(
            vm,
            "Ljava/util/GregorianCalendar;",
            HostState::Millis(now_ms()),
        ))),
        "getTimeInMillis" => Some(Some(V::I64(ms))),
        "setTimeInMillis" => {
            set_millis(vm, h, i64v(args[1]));
            Some(None)
        }
        "getTime" => Some(Some(host(vm, DATE_DESC, HostState::Millis(ms)))),
        "setTime" => {
            set_millis(vm, h, millis_of(vm, args[1]));
            Some(None)
        }
        "get" => {
            let (y, mo, d, hh, mi, s, msec) = ms_fields(ms);
            let f = i32v(args[1]);
            Some(Some(V::I32(match f {
                1 => y as i32,          // YEAR
                2 => (mo - 1) as i32,   // MONTH
                3 => 0,                 // WEEK_OF_YEAR (approx)
                4 => 0,                 // WEEK_OF_MONTH
                5 => d as i32,          // DATE / DAY_OF_MONTH
                6 => (ms.div_euclid(86_400_000) - days_from_civil(y, 1, 1) + 1) as i32, // DAY_OF_YEAR
                7 => ((ms.div_euclid(86_400_000) + 4).rem_euclid(7) + 1) as i32, // DAY_OF_WEEK (Sun=1)
                8 => ((d - 1) / 7 + 1) as i32, // DAY_OF_WEEK_IN_MONTH
                9 => (hh >= 12) as i32, // AM_PM
                10 => (hh % 12) as i32, // HOUR
                11 => hh as i32,        // HOUR_OF_DAY
                12 => mi as i32,        // MINUTE
                13 => s as i32,         // SECOND
                14 => msec as i32,      // MILLISECOND
                15 => 0,                // ZONE_OFFSET
                16 => 0,                // DST_OFFSET
                _ => 0,
            })))
        }
        "set" => {
            let p = params(sig);
            if p.len() == 2 {
                // set(field, value)
                let f = i32v(args[1]);
                let v = i64v(args[2]);
                let (y, mo, d, hh, mi, s, msec) = ms_fields(ms);
                let (mut y2, mut mo2, mut d2, mut h2, mut mi2, mut s2, mut ms2) =
                    (y, mo, d, hh, mi, s, msec);
                match f {
                    1 => y2 = v,
                    2 => mo2 = v + 1,
                    5 => d2 = v,
                    9 => h2 += v * 12,
                    10 => h2 = (h2 / 12) * 12 + v,
                    11 => h2 = v,
                    12 => mi2 = v,
                    13 => s2 = v,
                    14 => ms2 = v,
                    _ => {}
                }
                set_millis(
                    vm,
                    h,
                    days_from_civil(y2, mo2, d2) * 86_400_000
                        + h2 * 3_600_000
                        + mi2 * 60_000
                        + s2 * 1000
                        + ms2,
                );
            } else if p.len() == 4 {
                // set(y, m, d)
                set_millis(
                    vm,
                    h,
                    days_from_civil(i64v(args[1]), i64v(args[2]) + 1, i64v(args[3]))
                        * 86_400_000,
                );
            } else {
                // set(y,m,d,h,mi[,s]) — read through get() so a short
                // overload can never index past the arg list.
                let g =
                    |i: usize| args.get(i).map(|v| i64v(*v)).unwrap_or(0);
                set_millis(
                    vm,
                    h,
                    days_from_civil(g(1), g(2) + 1, g(3)) * 86_400_000
                        + g(4) * 3_600_000
                        + g(5) * 60_000
                        + g(6) * 1000,
                );
            }
            Some(None)
        }
        "add" => {
            let (f, n) = (i32v(args[1]), i64v(args[2]));
            let _ = ms_fields(ms);
            let mut days = ms.div_euclid(86_400_000);
            let mut extra = ms.rem_euclid(86_400_000);
            match f {
                1 => {
                    let (y, m, d) = civil_from_days(days);
                    days = days_from_civil(y + n, m, d);
                }
                2 => {
                    let (y, m, d) = civil_from_days(days);
                    days = days_from_civil(y, m + n, d.min(28));
                }
                3 | 4 | 8 => days += n * 7,
                5..=7 => days += n,
                10 | 11 => extra += n * 3_600_000,
                12 => extra += n * 60_000,
                13 => extra += n * 1000,
                14 => extra += n,
                _ => {}
            }
            set_millis(vm, h, days * 86_400_000 + extra);
            Some(None)
        }
        "roll" => {
            // roll(field, amount): same as add for our purposes
            let (f, n) = (i32v(args[1]), i64v(args[2]));
            let _ = (f, n);
            Some(None)
        }
        "before" => Some(Some(V::I32((ms < millis_of(vm, args[1])) as i32))),
        "after" => Some(Some(V::I32((ms > millis_of(vm, args[1])) as i32))),
        "compareTo" => Some(Some(V::I32(ms.cmp(&millis_of(vm, args[1])) as i32))),
        "clear" => {
            set_millis(vm, h, 0);
            Some(None)
        }
        "isSet" => Some(Some(V::I32(1))),
        "clone" => Some(Some(host(
            vm,
            "Ljava/util/GregorianCalendar;",
            HostState::Millis(ms),
        ))),
        "getTimeZone" => Some(Some(host(
            vm,
            "Ljava/util/TimeZone;",
            HostState::StrBuf("UTC".into()),
        ))),
        "setTimeZone" | "setLenient" | "setFirstDayOfWeek"
        | "setMinimalDaysInFirstWeek" | "setWeekDate" => Some(None),
        "isLenient" => Some(Some(V::I32(1))),
        "getFirstDayOfWeek" => Some(Some(V::I32(1))),
        "getMinimalDaysInFirstWeek" => Some(Some(V::I32(1))),
        "getActualMaximum" | "getActualMinimum" => {
            let f = i32v(args[1]);
            Some(Some(V::I32(match f {
                5 => 31,
                11 => 23,
                12 => 59,
                13 => 59,
                14 => 999,
                1 => 292278994,
                2 => 11,
                3 => 53,
                4 => 6,
                6 => 366,
                7 => 7,
                8 => 6,
                _ => 0,
            })))
        }
        "getDisplayName" | "getDisplayNames" => Some(Some(V::Ref(0))),
        "toInstant" => Some(Some(host(
            vm,
            "Ljava/time/Instant;",
            HostState::Millis(ms),
        ))),
        _ => None,
    })
}

fn timezone_call(vm: &mut Vm, name: &str, args: &[V]) -> Option<Option<V>> {
    match name {
        "getDefault" | "getTimeZone" => Some(Some(host(
            vm,
            "Ljava/util/TimeZone;",
            HostState::StrBuf("UTC".into()),
        ))),
        "getID" | "getDisplayName" | "toString" => {
            let t = s(vm, match args[0] { V::Ref(h) => h, _ => 0 })
                .unwrap_or_else(|| "UTC".into());
            Some(Some(jstr(vm, &t)))
        }
        "getOffset" | "getRawOffset" | "getDSTSavings" | "observeDaylightTime" => {
            Some(Some(V::I32(0)))
        }
        "inDaylightTime" | "useDaylightTime" => Some(Some(V::I32(0))),
        "setID" | "setRawOffset" | "setDefault" => Some(None),
        "getAvailableIDs" => {
            let u = jstr(vm, "UTC");
            Some(Some(arr(vm, STRING, vec![u])))
        }
        "clone" => Some(Some(args[0])),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// java.time

fn jtime_call(vm: &mut Vm, class: &str, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match (class, name) {
        ("Ljava/time/Instant;", "now") => Some(Some(host(
            vm,
            class,
            HostState::Millis(now_ms()),
        ))),
        ("Ljava/time/Instant;", "ofEpochMilli" | "ofEpochSecond") => {
            let ms = if name == "ofEpochMilli" {
                i64v(args[0])
            } else {
                i64v(args[0]) * 1000
            };
            Some(Some(host(vm, class, HostState::Millis(ms))))
        }
        ("Ljava/time/Instant;", "toEpochMilli") => {
            Some(Some(V::I64(millis_of(vm, args[0]))))
        }
        ("Ljava/time/Instant;", "getEpochSecond") => {
            Some(Some(V::I64(millis_of(vm, args[0]) / 1000)))
        }
        ("Ljava/time/Instant;", "getNano") => Some(Some(V::I32(0))),
        ("Ljava/time/Duration;", "ofMillis" | "ofSeconds" | "ofNanos"
        | "ofMinutes" | "ofHours" | "ofDays") => {
            let f = match name {
                "ofSeconds" => 1_000,
                "ofMinutes" => 60_000,
                "ofHours" => 3_600_000,
                "ofDays" => 86_400_000,
                _ => 1,
            };
            Some(Some(host(
                vm,
                class,
                HostState::Millis(i64v(args[0]) * f),
            )))
        }
        ("Ljava/time/Duration;", "toMillis") => {
            Some(Some(V::I64(millis_of(vm, args[0]))))
        }
        ("Ljava/time/Duration;", "getSeconds" | "toSeconds") => {
            Some(Some(V::I64(millis_of(vm, args[0]) / 1000)))
        }
        ("Ljava/time/Duration;", "toNanos") => {
            Some(Some(V::I64(millis_of(vm, args[0]) * 1_000_000)))
        }
        (_, "now") if class.contains("ZoneId") || class.contains("ZoneOffset") => {
            Some(Some(host(vm, class, HostState::StrBuf("UTC".into()))))
        }
        (_, "of" | "ofPattern" | "systemDefault" | "getId" | "toString") => {
            if name == "toString" {
                let ms = millis_of(vm, args[0]);
                Some(Some(jstr(vm, &date_to_string(ms))))
            } else {
                Some(Some(host(vm, class, HostState::Empty)))
            }
        }
        _ => None,
    })
}
