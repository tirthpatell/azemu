//! Date and time functions. Date/times are ISO 8601 strings; arithmetic is done on ticks.

use super::{f, FuncSpec};
use crate::datetime::{format_custom, parse_datetime, Dt, Part, TICKS_PER_DAY};
use crate::eval::Ctx;
use crate::value::Val;

pub(super) static FUNCS: &[FuncSpec] = &[
    f("GETCURRENTDATETIME", 0, 0, |c, _| dt_val(Dt::from_ticks(c.now_ticks(false)))),
    f("GETCURRENTDATETIMESTATIC", 0, 0, |c, _| dt_val(Dt::from_ticks(c.now_ticks(true)))),
    f("GETCURRENTTICKS", 0, 0, |c, _| Val::int(c.now_ticks(false))),
    f("GETCURRENTTICKSSTATIC", 0, 0, |c, _| Val::int(c.now_ticks(true))),
    f("GETCURRENTTIMESTAMP", 0, 0, |c, _| Val::int(c.now_ticks(false).div_euclid(10_000))),
    f("GETCURRENTTIMESTAMPSTATIC", 0, 0, |c, _| Val::int(c.now_ticks(true).div_euclid(10_000))),
    f("NOW", 0, 2, |c, a| now_ago(c, a, 1)),
    f("AGO", 1, 2, |c, a| now_ago(c, a, -1)),
    f("DATETIMEADD", 3, 3, datetime_add),
    f("DATETIMEDIFF", 3, 3, datetime_diff),
    f("DATETIMEFROMPARTS", 3, 7, from_parts),
    f("DATETIMEPART", 2, 2, datetime_part),
    f("DATETIMEBIN", 2, 4, datetime_bin),
    f("DATETIMETOTICKS", 1, 1, |_, a| with_dt(&a[0], |d| Val::int(d.ticks()))),
    f("DATETIMETOTIMESTAMP", 1, 1, |_, a| with_dt(&a[0], |d| Val::int(d.ticks().div_euclid(10_000)))),
    f("TICKSTODATETIME", 1, 1, |_, a| match a[0].as_i64() {
        Some(t) => dt_val(Dt::from_ticks(t)),
        None => Val::Undefined,
    }),
    f("TIMESTAMPTODATETIME", 1, 1, |_, a| match a[0].as_i64().and_then(|t| t.checked_mul(10_000)) {
        Some(t) => dt_val(Dt::from_ticks(t)),
        None => Val::Undefined,
    }),
    f("YEAR", 1, 1, |_, a| with_dt(&a[0], |d| Val::int(d.year))),
    f("MONTH", 1, 1, |_, a| with_dt(&a[0], |d| Val::int(d.month as i64))),
    f("DAY", 1, 1, |_, a| with_dt(&a[0], |d| Val::int(d.day as i64))),
    f("IS_DATETIME", 1, 1, |_, a| match &a[0] {
        Val::Str(s) => Val::Bool(parse_datetime(s).is_some()),
        _ => Val::Bool(false),
    }),
    f("DATETIMEFORMAT", 2, 2, |_, a| match (&a[0], &a[1]) {
        (Val::Str(s), Val::Str(fmt)) => match parse_datetime(s) {
            Some(d) => Val::string(format_custom(&d, fmt)),
            None => Val::Undefined,
        },
        _ => Val::Undefined,
    }),
];

fn dt_val(d: Option<Dt>) -> Val {
    match d {
        Some(d) => Val::string(d.format()),
        None => Val::Undefined,
    }
}

fn with_dt(v: &Val, op: impl Fn(&Dt) -> Val) -> Val {
    match v {
        Val::Str(s) => parse_datetime(s).map(|d| op(&d)).unwrap_or(Val::Undefined),
        _ => Val::Undefined,
    }
}

fn part_of(v: &Val) -> Option<Part> {
    Part::parse(v.as_str()?)
}

fn add_part(dt: &Dt, part: Part, n: i64) -> Option<Dt> {
    match part {
        Part::Year => dt.add_months(n.checked_mul(12)?),
        Part::Month => dt.add_months(n),
        _ => {
            let nanos = part.nanos()?.checked_mul(n as i128)?;
            let ticks = (dt.ticks() as i128).checked_add(nanos / 100)?;
            Dt::from_ticks(i64::try_from(ticks).ok()?)
        }
    }
}

/// `3d` / `-3year` style time span.
fn parse_span(s: &str) -> Option<(Part, i64)> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let n: i64 = s[..split].trim().parse().ok()?;
    Some((Part::parse(s[split..].trim())?, n))
}

fn now_ago(ctx: &Ctx, a: &[Val], sign: i64) -> Val {
    let now = match Dt::from_ticks(ctx.now_ticks(false)) {
        Some(d) => d,
        None => return Val::Undefined,
    };
    let (part, n) = match a.len() {
        0 => return dt_val(Some(now)),
        1 => match a[0].as_str().and_then(parse_span) {
            Some(x) => x,
            None => return Val::Undefined,
        },
        _ => match (part_of(&a[0]), a[1].as_i64()) {
            (Some(p), Some(n)) => (p, n),
            _ => return Val::Undefined,
        },
    };
    match n.checked_mul(sign) {
        Some(n) => dt_val(add_part(&now, part, n)),
        None => Val::Undefined,
    }
}

fn datetime_add(_: &Ctx, a: &[Val]) -> Val {
    let (part, n, dt) = match (part_of(&a[0]), a[1].as_i64(), a[2].as_str().and_then(parse_datetime)) {
        (Some(p), Some(n), Some(d)) => (p, n, d),
        _ => return Val::Undefined,
    };
    dt_val(add_part(&dt, part, n))
}

fn unit_ticks(part: Part) -> Option<i64> {
    Some(match part {
        Part::Day => TICKS_PER_DAY,
        Part::Hour => 36_000_000_000,
        Part::Minute => 600_000_000,
        Part::Second => 10_000_000,
        Part::Millisecond => 10_000,
        Part::Microsecond => 10,
        _ => return None,
    })
}

fn datetime_diff(_: &Ctx, a: &[Val]) -> Val {
    let (part, d1, d2) = match (part_of(&a[0]), a[1].as_str().and_then(parse_datetime), a[2].as_str().and_then(parse_datetime)) {
        (Some(p), Some(x), Some(y)) => (p, x, y),
        _ => return Val::Undefined,
    };
    Val::int(match part {
        Part::Year => d2.year - d1.year,
        Part::Month => (d2.year * 12 + d2.month as i64) - (d1.year * 12 + d1.month as i64),
        Part::Nanosecond => (d2.ticks() - d1.ticks()).saturating_mul(100),
        p => match unit_ticks(p) {
            Some(u) => d2.ticks().div_euclid(u) - d1.ticks().div_euclid(u),
            None => return Val::Undefined,
        },
    })
}

fn from_parts(_: &Ctx, a: &[Val]) -> Val {
    let mut n = [0u32; 7];
    n[1] = 1;
    n[2] = 1;
    for (i, v) in a.iter().enumerate() {
        n[i] = match v.as_i64().and_then(|x| u32::try_from(x).ok()) {
            Some(x) => x,
            None => return Val::Undefined,
        };
    }
    dt_val(Dt::new(n[0] as i64, n[1], n[2], n[3], n[4], n[5], n[6]))
}

fn datetime_part(_: &Ctx, a: &[Val]) -> Val {
    let (part, d) = match (part_of(&a[0]), a[1].as_str().and_then(parse_datetime)) {
        (Some(p), Some(d)) => (p, d),
        _ => return Val::Undefined,
    };
    Val::int(match part {
        Part::Year => d.year,
        Part::Month => d.month as i64,
        Part::Day => d.day as i64,
        Part::Hour => d.hour as i64,
        Part::Minute => d.minute as i64,
        Part::Second => d.second as i64,
        Part::Millisecond => (d.frac / 10_000) as i64,
        Part::Microsecond => (d.frac / 10) as i64,
        Part::Nanosecond => d.frac as i64 * 100,
    })
}

fn datetime_bin(_: &Ctx, a: &[Val]) -> Val {
    let (dt, part) = match (a[0].as_str().and_then(parse_datetime), part_of(&a[1])) {
        (Some(d), Some(p)) => (d, p),
        _ => return Val::Undefined,
    };
    let size = match a.get(2) {
        None => 1,
        Some(v) => match v.as_i64() {
            Some(s) if s > 0 => s,
            _ => return Val::Undefined,
        },
    };
    let start = match a.get(3) {
        None => Dt::new(1970, 1, 1, 0, 0, 0, 0),
        Some(v) => v.as_str().and_then(parse_datetime),
    };
    let start = match start {
        Some(s) if s.year >= 1601 => s,
        _ => return Val::Undefined,
    };
    match part {
        Part::Year => {
            let yb = start.year + (dt.year - start.year).div_euclid(size) * size;
            dt_val(Dt::new(yb, 1, 1, 0, 0, 0, 0))
        }
        Part::Month => {
            let idx = (dt.year * 12 + dt.month as i64 - 1) - (start.year * 12 + start.month as i64 - 1);
            let total = start.year * 12 + start.month as i64 - 1 + idx.div_euclid(size) * size;
            dt_val(Dt::new(total.div_euclid(12), (total.rem_euclid(12) + 1) as u32, 1, 0, 0, 0, 0))
        }
        p => {
            let unit = match p.nanos().and_then(|u| u.checked_mul(size as i128)) {
                Some(u) => u,
                None => return Val::Undefined,
            };
            let t = dt.ticks() as i128 * 100;
            let t0 = start.ticks() as i128 * 100;
            let b = t0 + (t - t0).div_euclid(unit) * unit;
            match i64::try_from(b.div_euclid(100)) {
                Ok(ticks) => dt_val(Dt::from_ticks(ticks)),
                Err(_) => Val::Undefined,
            }
        }
    }
}
