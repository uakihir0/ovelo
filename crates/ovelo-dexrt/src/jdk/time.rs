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
    Ok(
        if desc_is_a(class, DATE_DESC) || class.starts_with("Ljava/sql/") {
            date_call(vm, name, args)?
        } else if desc_is_a(class, "Ljava/util/GregorianCalendar;") || desc_is_a(class, CAL_DESC) {
            cal_call(vm, name, sig, args)?
        } else if desc_is_a(class, "Ljava/util/TimeZone;") {
            timezone_call(vm, name, args)
        } else if class.starts_with("Ljava/time/") {
            jtime_call(vm, class, name, args)?
        } else {
            return Ok(None);
        },
    )
}

// ---------------------------------------------------------------------
// civil date math (Howard Hinnant algorithms)

pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
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

pub(crate) fn ms_fields(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
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
                let g = |i: usize| args.get(i).map(|v| i64v(*v)).unwrap_or(0);
                let v = match args.len() {
                    0 | 1 => now_ms(),
                    // Date(String) parses; Date(long) and Date(Date)
                    // take the millis.
                    2 => match args[1] {
                        V::Ref(r) => s(vm, r)
                            .map(|t| parse_date(&t))
                            .unwrap_or_else(|| millis_of(vm, args[1])),
                        _ => g(1),
                    },
                    _ => {
                        days_from_civil(g(1) + 1900, g(2) + 1, g(3)) * 86_400_000
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
        "toInstant" => Some(Some(host(vm, "Ljava/time/Instant;", HostState::Millis(ms)))),
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
                "getAvailableLocales" | "getAvailableCalendarTypes" => {
                    Some(Some(arr(vm, "Ljava/util/Locale;", vec![])))
                }
                _ => None,
            });
        }
    };
    let ms = millis_of(vm, args[0]);
    Ok(match name {
        "<init>" => {
            // GregorianCalendar() / (TimeZone) / (Locale) /
            // (TimeZone, Locale) -> now; (y,m,d[,h,mi[,s]]) -> civil.
            // The object-arg overloads must not land in the civil
            // branch — i64v(ref) is 0, which would pin the calendar at
            // year 0.
            let g = |i: usize| args.get(i).map(|v| i64v(*v)).unwrap_or(0);
            let numeric_args = args
                .get(1)
                .is_some_and(|v| matches!(v, V::I32(_) | V::I64(_)));
            let v = if numeric_args {
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
                1 => y as i32,                                                          // YEAR
                2 => (mo - 1) as i32,                                                   // MONTH
                3 => 0,        // WEEK_OF_YEAR (approx)
                4 => 0,        // WEEK_OF_MONTH
                5 => d as i32, // DATE / DAY_OF_MONTH
                6 => (ms.div_euclid(86_400_000) - days_from_civil(y, 1, 1) + 1) as i32, // DAY_OF_YEAR
                7 => ((ms.div_euclid(86_400_000) + 4).rem_euclid(7) + 1) as i32, // DAY_OF_WEEK (Sun=1)
                8 => ((d - 1) / 7 + 1) as i32, // DAY_OF_WEEK_IN_MONTH
                9 => (hh >= 12) as i32,        // AM_PM
                10 => (hh % 12) as i32,        // HOUR
                11 => hh as i32,               // HOUR_OF_DAY
                12 => mi as i32,               // MINUTE
                13 => s as i32,                // SECOND
                14 => msec as i32,             // MILLISECOND
                15 => 0,                       // ZONE_OFFSET
                16 => 0,                       // DST_OFFSET
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
                    days_from_civil(i64v(args[1]), i64v(args[2]) + 1, i64v(args[3])) * 86_400_000,
                );
            } else {
                // set(y,m,d,h,mi[,s]) — read through get() so a short
                // overload can never index past the arg list.
                let g = |i: usize| args.get(i).map(|v| i64v(*v)).unwrap_or(0);
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
        "setTimeZone"
        | "setLenient"
        | "setFirstDayOfWeek"
        | "setMinimalDaysInFirstWeek"
        | "setWeekDate" => Some(None),
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
        "toInstant" => Some(Some(host(vm, "Ljava/time/Instant;", HostState::Millis(ms)))),
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
            let t = s(
                vm,
                match args[0] {
                    V::Ref(h) => h,
                    _ => 0,
                },
            )
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
        ("Ljava/time/Instant;", "now") => Some(Some(host(vm, class, HostState::Millis(now_ms())))),
        ("Ljava/time/Instant;", "ofEpochMilli" | "ofEpochSecond") => {
            let ms = if name == "ofEpochMilli" {
                i64v(args[0])
            } else {
                i64v(args[0]) * 1000
            };
            Some(Some(host(vm, class, HostState::Millis(ms))))
        }
        ("Ljava/time/Instant;", "toEpochMilli") => Some(Some(V::I64(millis_of(vm, args[0])))),
        ("Ljava/time/Instant;", "getEpochSecond") => {
            Some(Some(V::I64(millis_of(vm, args[0]) / 1000)))
        }
        ("Ljava/time/Instant;", "getNano") => Some(Some(V::I32(0))),
        (
            "Ljava/time/Duration;",
            "ofMillis" | "ofSeconds" | "ofNanos" | "ofMinutes" | "ofHours" | "ofDays",
        ) => {
            let f = match name {
                "ofSeconds" => 1_000,
                "ofMinutes" => 60_000,
                "ofHours" => 3_600_000,
                "ofDays" => 86_400_000,
                _ => 1,
            };
            Some(Some(host(vm, class, HostState::Millis(i64v(args[0]) * f))))
        }
        ("Ljava/time/Duration;", "toMillis") => Some(Some(V::I64(millis_of(vm, args[0])))),
        ("Ljava/time/Duration;", "getSeconds" | "toSeconds") => {
            Some(Some(V::I64(millis_of(vm, args[0]) / 1000)))
        }
        ("Ljava/time/Duration;", "toNanos") => {
            Some(Some(V::I64(millis_of(vm, args[0]) * 1_000_000)))
        }
        ("Ljava/time/Month;", "getValue") => Some(Some(V::I32(month_of(vm, args[0])))),
        ("Ljava/time/Month;", "of") => Some(Some(month_obj(
            vm,
            i64v(args.first().copied().unwrap_or(V::I32(1))),
        ))),
        ("Ljava/time/Month;", "values") => {
            let vals = (1..=12).map(|m| month_obj(vm, m)).collect();
            Some(Some(arr(vm, "Ljava/time/Month;", vals)))
        }
        ("Ljava/time/Month;", "valueOf") => {
            let n = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
            let Some(i) = MONTH_NAMES.iter().position(|m| *m == n) else {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IllegalArgumentException;",
                    &format!("No enum constant java.time.Month.{n}"),
                ));
            };
            Some(Some(month_obj(vm, i as i64 + 1)))
        }
        ("Ljava/time/Month;", "plus" | "minus") => {
            let s = if name == "plus" { 1 } else { -1 };
            Some(Some(month_obj(
                vm,
                month_of(vm, args[0]) as i64 + s * i64v(args.get(1).copied().unwrap_or(V::I64(0))),
            )))
        }
        ("Ljava/time/DayOfWeek;", "getValue") => Some(Some(V::I32(day_of_week_of(vm, args[0])))),
        ("Ljava/time/DayOfWeek;", "of") => Some(Some(day_of_week_obj(
            vm,
            i64v(args.first().copied().unwrap_or(V::I32(1))),
        ))),
        ("Ljava/time/DayOfWeek;", "values") => {
            let vals = (1..=7).map(|d| day_of_week_obj(vm, d)).collect();
            Some(Some(arr(vm, "Ljava/time/DayOfWeek;", vals)))
        }
        ("Ljava/time/DayOfWeek;", "valueOf") => {
            let n = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
            let Some(i) = DAY_NAMES.iter().position(|d| *d == n) else {
                return Err(vm.throw_builtin(
                    "Ljava/lang/IllegalArgumentException;",
                    &format!("No enum constant java.time.DayOfWeek.{n}"),
                ));
            };
            Some(Some(day_of_week_obj(vm, i as i64 + 1)))
        }
        ("Ljava/time/DayOfWeek;", "plus" | "minus") => {
            let s = if name == "plus" { 1 } else { -1 };
            Some(Some(day_of_week_obj(
                vm,
                day_of_week_of(vm, args[0]) as i64
                    + s * i64v(args.get(1).copied().unwrap_or(V::I64(0))),
            )))
        }
        // Enum API on the two java.time enums — they carry the
        // `Ljava/lang/Enum;` super but route here (lang excludes
        // java.time), so name()/ordinal()/compareTo need local arms.
        ("Ljava/time/Month;" | "Ljava/time/DayOfWeek;", "name" | "toString") => {
            let h = match args[0] {
                V::Ref(h) => h,
                _ => 0,
            };
            let n = pseudo_get(vm, h, P_DATA)
                .and_then(|v| sv(vm, v))
                .unwrap_or_default();
            Some(Some(jstr(vm, &n)))
        }
        ("Ljava/time/Month;" | "Ljava/time/DayOfWeek;", "ordinal") => Some(Some(match args[0] {
            V::Ref(h) => pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0)),
            _ => V::I32(0),
        })),
        ("Ljava/time/Month;" | "Ljava/time/DayOfWeek;", "compareTo") => {
            let ord = |v: V| match v {
                V::Ref(h) => i64v(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0))),
                _ => 0,
            };
            let d = ord(args[0]) - ord(*args.get(1).unwrap_or(&V::I32(0)));
            Some(Some(V::I32(d.signum() as i32)))
        }
        ("Ljava/time/Month;" | "Ljava/time/DayOfWeek;", "equals" | "hashCode") => {
            let a = args[0];
            Some(Some(if name == "equals" {
                V::I32((a == *args.get(1).unwrap_or(&V::I32(0))) as i32)
            } else {
                match a {
                    V::Ref(h) => pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0)),
                    _ => V::I32(0),
                }
            }))
        }
        ("Ljava/time/Month;" | "Ljava/time/DayOfWeek;", "getDeclaringClass") => {
            Some(Some(class_obj(vm, class.into())))
        }
        ("Ljava/time/LocalDate;", _) => localdate_call(vm, name, args)?,
        ("Ljava/time/LocalTime;", _) => localtime_call(vm, name, args)?,
        ("Ljava/time/LocalDateTime;", _) => localdatetime_call(vm, name, args)?,
        ("Ljava/time/ZoneOffset;", "ofTotalSeconds") => Some(Some(host(
            vm,
            class,
            HostState::Millis(i64v(args.first().copied().unwrap_or(V::I64(0))) * 1000),
        ))),
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

// java.time enum-like value objects (Month / DayOfWeek are real enums;
// minted as host objects carrying name + ordinal like dex enums).

const MONTH_NAMES: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];
const DAY_NAMES: [&str; 7] = [
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
    "SUNDAY",
];

/// Mint a java.time enum constant: StrBuf name + P_FLAGS ordinal —
/// the slots Enum.name()/ordinal()/compareTo read. Interned in
/// `vm.enum_cache` so `date.getMonth() == Month.JANUARY` holds.
pub(crate) fn jtime_enum(vm: &mut Vm, desc: &str, name: &str, ordinal: i32) -> V {
    if let Some(&h) = vm.enum_cache.get(&(desc.to_string(), ordinal)) {
        return V::Ref(h);
    }
    let v = host(vm, desc, HostState::StrBuf(name.into()));
    if let V::Ref(h) = v {
        let nm = jstr(vm, name);
        pseudo_set(vm, h, P_DATA, nm);
        pseudo_set(vm, h, P_FLAGS, V::I32(ordinal));
        vm.enum_cache.insert((desc.to_string(), ordinal), h);
    }
    v
}

/// Ordinal of a Month/DayOfWeek constant by name — used by the
/// self-typed static field path (`Month.JANUARY`) so constants carry
/// the right ordinal and share handles with `Month.of(1)` results.
pub(crate) fn jtime_enum_ordinal(owner: &str, name: &str) -> Option<i32> {
    match owner {
        "Ljava/time/Month;" => MONTH_NAMES
            .iter()
            .position(|n| *n == name)
            .map(|i| i as i32),
        "Ljava/time/DayOfWeek;" => DAY_NAMES.iter().position(|n| *n == name).map(|i| i as i32),
        _ => None,
    }
}

fn month_obj(vm: &mut Vm, m1_12: i64) -> V {
    let m = ((m1_12 - 1).rem_euclid(12)) as usize;
    jtime_enum(vm, "Ljava/time/Month;", MONTH_NAMES[m], m as i32)
}

fn day_of_week_obj(vm: &mut Vm, v1_7: i64) -> V {
    let d = ((v1_7 - 1).rem_euclid(7)) as usize;
    jtime_enum(vm, "Ljava/time/DayOfWeek;", DAY_NAMES[d], d as i32)
}

fn month_of(vm: &Vm, v: V) -> i32 {
    match v {
        V::Ref(h) => i32v(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0))) + 1,
        _ => i32v(v),
    }
}

fn day_of_week_of(vm: &Vm, v: V) -> i32 {
    match v {
        V::Ref(h) => i32v(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::I32(0))) + 1,
        _ => i32v(v),
    }
}

/// Days since epoch of an epoch-millis LocalDate/LocalDateTime.
fn ldate_days(ms: i64) -> i64 {
    ms.div_euclid(86_400_000)
}

/// Zone offset in ms carried by a ZoneId/ZoneOffset host object —
/// offsets minted as `Millis` (ofTotalSeconds); named zones are UTC
/// in this runtime so everything else reads as 0.
fn zone_off_ms(vm: &Vm, v: V) -> i64 {
    match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::Millis(o),
                ..
            }) => *o,
            _ => 0,
        },
        _ => 0,
    }
}

/// Days in a month (1-based) for a year.
fn dim(y: i64, m: i64) -> i64 {
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][(m - 1).clamp(0, 11) as usize]
}

fn localdate_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    // Static factories have no receiver — args[0] is the first param.
    if matches!(
        name,
        "now" | "ofInstant" | "of" | "ofEpochDay" | "ofYearDay" | "parse"
    ) {
        return Ok(match name {
            "now" => Some(Some(host(
                vm,
                "Ljava/time/LocalDate;",
                HostState::Millis(ldate_days(now_ms()) * 86_400_000),
            ))),
            "ofInstant" => {
                // ofInstant(Instant, ZoneId) — apply the zone offset so
                // dates near midnight land on the right day.
                let ms = millis_of(vm, *args.first().unwrap_or(&V::I64(0)))
                    + zone_off_ms(vm, args.get(1).copied().unwrap_or(V::I64(0)));
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalDate;",
                    HostState::Millis(ldate_days(ms) * 86_400_000),
                )))
            }
            "of" => {
                // of(year, month|Month, day) — month may be the enum.
                let mv = match args.get(1) {
                    Some(V::Ref(h)) => month_of(vm, V::Ref(*h)) as i64,
                    _ => i64v(args.get(1).copied().unwrap_or(V::I32(1))),
                };
                let ms = days_from_civil(
                    i64v(args.first().copied().unwrap_or(V::I32(1970))),
                    mv,
                    i64v(args.get(2).copied().unwrap_or(V::I32(1))),
                ) * 86_400_000;
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalDate;",
                    HostState::Millis(ms),
                )))
            }
            "ofEpochDay" => Some(Some(host(
                vm,
                "Ljava/time/LocalDate;",
                HostState::Millis(i64v(args.first().copied().unwrap_or(V::I64(0))) * 86_400_000),
            ))),
            "ofYearDay" => {
                let y = i64v(args.first().copied().unwrap_or(V::I32(1970)));
                let doy = i64v(args.get(1).copied().unwrap_or(V::I32(1)));
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalDate;",
                    HostState::Millis((days_from_civil(y, 1, 1) + doy - 1) * 86_400_000),
                )))
            }
            _ => {
                let t = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalDate;",
                    HostState::Millis(ldate_days(parse_date(&t)) * 86_400_000),
                )))
            }
        });
    }
    let ms = millis_of(vm, args[0]);
    let (y, mo, d, ..) = ms_fields(ms);
    Ok(match name {
        "getYear" => Some(Some(V::I32(y as i32))),
        "getMonth" => Some(Some(month_obj(vm, mo))),
        "getMonthValue" => Some(Some(V::I32(mo as i32))),
        "getDayOfMonth" => Some(Some(V::I32(d as i32))),
        "getDayOfYear" => Some(Some(V::I32(
            (ldate_days(ms) - days_from_civil(y, 1, 1) + 1) as i32,
        ))),
        "getDayOfWeek" => {
            // epoch day = Thursday (value 4): dow = ((days + 3) mod 7) + 1
            let dow = ((ldate_days(ms) + 3).rem_euclid(7)) + 1;
            Some(Some(day_of_week_obj(vm, dow)))
        }
        "toEpochDay" => Some(Some(V::I64(ldate_days(ms)))),
        "isLeapYear" => Some(Some(V::I32(
            (y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)) as i32,
        ))),
        "lengthOfYear" => Some(Some(V::I32(
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                366
            } else {
                365
            },
        ))),
        "lengthOfMonth" => Some(Some(V::I32(dim(y, mo) as i32))),
        "plusDays" | "minusDays" => {
            let s = if name.starts_with("plus") { 1 } else { -1 };
            Some(Some(host(
                vm,
                "Ljava/time/LocalDate;",
                HostState::Millis(
                    ms + s * i64v(args.get(1).copied().unwrap_or(V::I64(0))) * 86_400_000,
                ),
            )))
        }
        "plusWeeks" | "minusWeeks" => {
            let s = if name.starts_with("plus") { 1 } else { -1 };
            Some(Some(host(
                vm,
                "Ljava/time/LocalDate;",
                HostState::Millis(
                    ms + s * i64v(args.get(1).copied().unwrap_or(V::I64(0))) * 7 * 86_400_000,
                ),
            )))
        }
        "plusMonths" | "minusMonths" => {
            let s = if name.starts_with("plus") { 1 } else { -1 };
            let n = s * i64v(args.get(1).copied().unwrap_or(V::I64(0)));
            let total = (y * 12 + mo - 1) + n;
            let (ny, nm0) = (total.div_euclid(12), total.rem_euclid(12));
            Some(Some(host(
                vm,
                "Ljava/time/LocalDate;",
                HostState::Millis(
                    days_from_civil(ny, nm0 + 1, d.min(dim(ny, nm0 + 1))) * 86_400_000,
                ),
            )))
        }
        "plusYears" | "minusYears" => {
            let s = if name.starts_with("plus") { 1 } else { -1 };
            let ny = y + s * i64v(args.get(1).copied().unwrap_or(V::I64(0)));
            Some(Some(host(
                vm,
                "Ljava/time/LocalDate;",
                HostState::Millis(days_from_civil(ny, mo, d.min(dim(ny, mo))) * 86_400_000),
            )))
        }
        "isBefore" => Some(Some(V::I32((ms < millis_of(vm, args[1])) as i32))),
        "isAfter" => Some(Some(V::I32((ms > millis_of(vm, args[1])) as i32))),
        "isEqual" | "equals" => Some(Some(V::I32((ms == millis_of(vm, args[1])) as i32))),
        "compareTo" => Some(Some(V::I32(ms.cmp(&millis_of(vm, args[1])) as i32))),
        "hashCode" => {
            // java.time.LocalDate.hashCode: (y & -2048) ^ (y<<11)+(mo<<6)+d
            Some(Some(V::I32(
                ((y & -2048) ^ ((y << 11) + (mo << 6) + d)) as i32,
            )))
        }
        "atTime" | "atStartOfDay" => Some(Some(host(
            vm,
            "Ljava/time/LocalDateTime;",
            HostState::Millis(ms),
        ))),
        "toString" => Some(Some(jstr(vm, &format!("{y:04}-{mo:02}-{d:02}")))),
        _ => None,
    })
}

fn localtime_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    // Static factories have no receiver — args[0] is the first param.
    if matches!(
        name,
        "now" | "of" | "ofSecondOfDay" | "ofNanoOfDay" | "parse"
    ) {
        return Ok(match name {
            "now" => Some(Some(host(
                vm,
                "Ljava/time/LocalTime;",
                HostState::Millis(now_ms().rem_euclid(86_400_000)),
            ))),
            "of" | "ofSecondOfDay" | "ofNanoOfDay" => {
                let ms = match name {
                    "of" => {
                        i64v(args.first().copied().unwrap_or(V::I32(0))) * 3_600_000
                            + i64v(args.get(1).copied().unwrap_or(V::I32(0))) * 60_000
                            + i64v(args.get(2).copied().unwrap_or(V::I32(0))) * 1000
                            + i64v(args.get(3).copied().unwrap_or(V::I32(0))) / 1_000_000
                    }
                    "ofSecondOfDay" => i64v(args.first().copied().unwrap_or(V::I64(0))) * 1000,
                    _ => i64v(args.first().copied().unwrap_or(V::I64(0))) / 1_000_000,
                };
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalTime;",
                    HostState::Millis(ms),
                )))
            }
            _ => {
                let t = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                let mut tp = t.split(':');
                let ms = tp.next().and_then(|x| x.parse::<i64>().ok()).unwrap_or(0) * 3_600_000
                    + tp.next().and_then(|x| x.parse::<i64>().ok()).unwrap_or(0) * 60_000
                    + tp.next()
                        .and_then(|x| x.split('.').next().and_then(|v| v.parse::<i64>().ok()))
                        .unwrap_or(0)
                        * 1000;
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalTime;",
                    HostState::Millis(ms),
                )))
            }
        });
    }
    // Stored as ms-of-day.
    let ms = millis_of(vm, args[0]);
    Ok(match name {
        "getHour" => Some(Some(V::I32((ms / 3_600_000) as i32))),
        "getMinute" => Some(Some(V::I32(((ms / 60_000) % 60) as i32))),
        "getSecond" => Some(Some(V::I32(((ms / 1000) % 60) as i32))),
        "getNano" => Some(Some(V::I32(((ms % 1000) * 1_000_000) as i32))),
        "toSecondOfDay" => Some(Some(V::I32((ms / 1000) as i32))),
        "toNanoOfDay" => Some(Some(V::I64(ms * 1_000_000))),
        "toString" => {
            let (h, mi, s) = (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60);
            Some(Some(jstr(
                vm,
                &if s == 0 {
                    format!("{h:02}:{mi:02}")
                } else {
                    format!("{h:02}:{mi:02}:{s:02}")
                },
            )))
        }
        _ => None,
    })
}

fn localdatetime_call(vm: &mut Vm, name: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    // Static factories have no receiver — args[0] is the first param.
    if matches!(name, "now" | "of" | "parse" | "ofInstant") {
        return Ok(match name {
            "now" => Some(Some(host(
                vm,
                "Ljava/time/LocalDateTime;",
                HostState::Millis(now_ms()),
            ))),
            "ofInstant" => Some(Some(host(
                vm,
                "Ljava/time/LocalDateTime;",
                HostState::Millis(millis_of(vm, *args.first().unwrap_or(&V::I64(0)))),
            ))),
            "of" => {
                let ms = days_from_civil(
                    i64v(args.first().copied().unwrap_or(V::I32(1970))),
                    match args.get(1) {
                        Some(V::Ref(h)) => month_of(vm, V::Ref(*h)) as i64,
                        _ => i64v(args.get(1).copied().unwrap_or(V::I32(1))),
                    },
                    i64v(args.get(2).copied().unwrap_or(V::I32(1))),
                ) * 86_400_000
                    + i64v(args.get(3).copied().unwrap_or(V::I32(0))) * 3_600_000
                    + i64v(args.get(4).copied().unwrap_or(V::I32(0))) * 60_000
                    + i64v(args.get(5).copied().unwrap_or(V::I32(0))) * 1000
                    + i64v(args.get(6).copied().unwrap_or(V::I32(0))) / 1_000_000;
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalDateTime;",
                    HostState::Millis(ms),
                )))
            }
            _ => {
                let t = sv(vm, args.first().copied().unwrap_or(V::Ref(0))).unwrap_or_default();
                Some(Some(host(
                    vm,
                    "Ljava/time/LocalDateTime;",
                    HostState::Millis(parse_date(&t)),
                )))
            }
        });
    }
    let ms = millis_of(vm, args[0]);
    let (y, mo, d, h, mi, s, n) = ms_fields(ms);
    Ok(match name {
        "getYear" => Some(Some(V::I32(y as i32))),
        "getMonth" => Some(Some(month_obj(vm, mo))),
        "getMonthValue" => Some(Some(V::I32(mo as i32))),
        "getDayOfMonth" => Some(Some(V::I32(d as i32))),
        "getDayOfYear" => Some(Some(V::I32(
            (ldate_days(ms) - days_from_civil(y, 1, 1) + 1) as i32,
        ))),
        "getDayOfWeek" => {
            let dow = ((ldate_days(ms) + 3).rem_euclid(7)) + 1;
            Some(Some(day_of_week_obj(vm, dow)))
        }
        "getHour" => Some(Some(V::I32(h as i32))),
        "getMinute" => Some(Some(V::I32(mi as i32))),
        "getSecond" => Some(Some(V::I32(s as i32))),
        "getNano" => Some(Some(V::I32((n * 1_000_000) as i32))),
        "toLocalDate" => Some(Some(host(
            vm,
            "Ljava/time/LocalDate;",
            HostState::Millis(ldate_days(ms) * 86_400_000),
        ))),
        "toLocalTime" => Some(Some(host(
            vm,
            "Ljava/time/LocalTime;",
            HostState::Millis(ms.rem_euclid(86_400_000)),
        ))),
        "toEpochSecond" | "toEpochMilli" => {
            // Optional ZoneOffset arg — Millis host carries offset ms.
            let off_ms = args
                .get(1)
                .and_then(|v| match v {
                    V::Ref(h) => match vm.heap.get(*h) {
                        Some(Obj::Host {
                            state: HostState::Millis(o),
                            ..
                        }) => Some(*o),
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or(0);
            let e = ms - off_ms;
            Some(Some(if name == "toEpochSecond" {
                V::I64(e / 1000)
            } else {
                V::I64(e)
            }))
        }
        "isBefore" => Some(Some(V::I32((ms < millis_of(vm, args[1])) as i32))),
        "isAfter" => Some(Some(V::I32((ms > millis_of(vm, args[1])) as i32))),
        "equals" => Some(Some(V::I32((ms == millis_of(vm, args[1])) as i32))),
        "compareTo" => Some(Some(V::I32(ms.cmp(&millis_of(vm, args[1])) as i32))),
        "plusDays" | "minusDays" | "plusHours" | "minusHours" | "plusMinutes" | "minusMinutes"
        | "plusSeconds" | "minusSeconds" => {
            let unit = if name.contains("Days") {
                86_400_000
            } else if name.contains("Hours") {
                3_600_000
            } else if name.contains("Minutes") {
                60_000
            } else {
                1000
            };
            let s = if name.starts_with("plus") { 1 } else { -1 };
            Some(Some(host(
                vm,
                "Ljava/time/LocalDateTime;",
                HostState::Millis(ms + s * i64v(args.get(1).copied().unwrap_or(V::I64(0))) * unit),
            )))
        }
        "toString" => Some(Some(jstr(
            vm,
            &format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}"),
        ))),
        _ => None,
    })
}
