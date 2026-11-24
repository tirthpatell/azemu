//! Dates for the template functions `utcNow`, `dateTimeAdd`, `dateTimeToEpoch` and `dateTimeFromEpoch`: parsing
//! ISO 8601 text and durations, and the .NET date and time format strings that those functions take
//! (<https://learn.microsoft.com/dotnet/standard/base-types/standard-date-and-time-format-strings> and
//! <https://learn.microsoft.com/dotnet/standard/base-types/custom-date-and-time-format-strings>).
//!
//! Every value is UTC. The names of days and months are the invariant (English) ones, as the documentation's
//! outputs show (`utcNow('d')` is `03/05/2019`). A format character that .NET does not know is an error that names
//! it: a date is never made with a guess.

const TICKS_PER_SEC: i64 = 10_000_000;
/// 0001-01-01T00:00:00Z and 9999-12-31T23:59:59Z as seconds since 1970 (the range of a .NET `DateTime`).
const MIN_SECS: i64 = -62_135_596_800;
const MAX_SECS: i64 = 253_402_300_799;

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// A UTC instant: whole seconds since 1970 plus 100-nanosecond ticks (0 to 9,999,999).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dt {
    pub secs: i64,
    pub ticks: u32,
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    }
}

/// The fields of an instant.
struct Parts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    ticks: i64,
    /// 0 = Sunday.
    weekday: usize,
}

impl Dt {
    fn parts(&self) -> Parts {
        let days = self.secs.div_euclid(86_400);
        let rem = self.secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Parts {
            year,
            month,
            day,
            hour: rem / 3600,
            minute: rem % 3600 / 60,
            second: rem % 60,
            ticks: i64::from(self.ticks),
            weekday: (days + 4).rem_euclid(7) as usize,
        }
    }

    pub fn from_epoch(secs: i64) -> Result<Dt, String> {
        if !(MIN_SECS..=MAX_SECS).contains(&secs) {
            return Err(format!("the epoch time {secs} is outside the years 1 to 9999."));
        }
        Ok(Dt { secs, ticks: 0 })
    }

    /// Add a duration of whole seconds and ticks (either may be negative).
    pub fn add(&self, secs: i64, ticks: i64) -> Result<Dt, String> {
        let total_ticks = i64::from(self.ticks) + ticks;
        let carry = total_ticks.div_euclid(TICKS_PER_SEC);
        let ticks = total_ticks.rem_euclid(TICKS_PER_SEC) as u32;
        let secs = self
            .secs
            .checked_add(secs)
            .and_then(|s| s.checked_add(carry))
            .filter(|s| (MIN_SECS..=MAX_SECS).contains(s))
            .ok_or("the result is outside the years 1 to 9999.")?;
        Ok(Dt { secs, ticks })
    }
}

fn digits(b: &[u8], from: usize, n: usize) -> Option<i64> {
    let s = b.get(from..from + n)?;
    if s.iter().all(u8::is_ascii_digit) {
        std::str::from_utf8(s).ok()?.parse().ok()
    } else {
        None
    }
}

/// ISO 8601 text: `2020-04-07 14:53:14Z`, `2023-01-01T00:00:00Z`, `2020-04-07T14:53:14.1234567+02:00`,
/// `2020-04-07` and the basic form `20200407T145314Z` (what `utcNow()` returns). Text with no zone is UTC.
pub fn parse(text: &str) -> Option<Dt> {
    let (wall, offset) = parse_parts(text)?;
    wall.add(-offset, 0).ok()
}

/// The date and time as they are written, with a zone offset not applied: `dateTimeAdd` of Azure keeps `12:00:00+02:00` as
/// 12:00 and writes it with a `Z` (checked against a real deployment), while `dateTimeToEpoch` uses the instant.
pub fn parse_wall(text: &str) -> Option<Dt> {
    parse_parts(text).map(|(w, _)| w)
}

/// The written date and time (as if UTC) and the zone offset in seconds.
fn parse_parts(text: &str) -> Option<(Dt, i64)> {
    let t = text.trim().as_bytes();
    let (y, mo, d, mut i, basic) = if t.len() >= 8 && t[..8].iter().all(u8::is_ascii_digit) {
        (digits(t, 0, 4)?, digits(t, 4, 2)?, digits(t, 6, 2)?, 8, true)
    } else {
        if t.get(4) != Some(&b'-') || t.get(7) != Some(&b'-') {
            return None;
        }
        (digits(t, 0, 4)?, digits(t, 5, 2)?, digits(t, 8, 2)?, 10, false)
    };
    if !(1..=9999).contains(&y) || !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    let (mut h, mut mi, mut s, mut ticks) = (0, 0, 0, 0i64);
    if i < t.len() && (t[i] == b'T' || t[i] == b't' || t[i] == b' ') {
        i += 1;
        h = digits(t, i, 2)?;
        i += 2;
        let colon = !basic;
        if colon {
            if t.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
        }
        mi = digits(t, i, 2)?;
        i += 2;
        if i < t.len() && ((colon && t[i] == b':') || (!colon && t[i].is_ascii_digit())) {
            if colon {
                i += 1;
            }
            s = digits(t, i, 2)?;
            i += 2;
            if i < t.len() && (t[i] == b'.' || t[i] == b',') {
                i += 1;
                let start = i;
                while i < t.len() && t[i].is_ascii_digit() {
                    i += 1;
                }
                if start == i || i - start > 9 {
                    return None;
                }
                let mut f = std::str::from_utf8(&t[start..i]).ok()?.to_string();
                f.truncate(7);
                while f.len() < 7 {
                    f.push('0');
                }
                ticks = f.parse().ok()?;
            }
        }
        if h > 23 || mi > 59 || s > 59 {
            return None;
        }
    }
    let mut offset = 0i64;
    if i < t.len() {
        match t[i] {
            b'Z' | b'z' => i += 1,
            b'+' | b'-' => {
                let sign = if t[i] == b'-' { -1 } else { 1 };
                i += 1;
                let oh = digits(t, i, 2)?;
                i += 2;
                if t.get(i) == Some(&b':') {
                    i += 1;
                }
                let om = if i < t.len() {
                    let m = digits(t, i, 2)?;
                    i += 2;
                    m
                } else {
                    0
                };
                if oh > 23 || om > 59 {
                    return None;
                }
                offset = sign * (oh * 3600 + om * 60);
            }
            _ => return None,
        }
    }
    if i != t.len() {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s;
    if !(MIN_SECS..=MAX_SECS).contains(&secs) {
        return None;
    }
    Some((Dt { secs, ticks: ticks as u32 }, offset))
}

/// An ISO 8601 duration with an optional leading `-` (or `+`): `P1Y2M3DT4H5M6.5S` (no weeks: Azure refuses `P1W`). A year is 365 days and a month
/// 30 days (Microsoft: "`dateTimeAdd` doesn't take leap years into consideration"). Returns whole seconds and ticks.
pub fn parse_duration(text: &str) -> Result<(i64, i64), String> {
    let bad = || format!("'{text}' is not an ISO 8601 duration such as 'P1D' or 'PT1H'.");
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let t = t.strip_prefix('P').or_else(|| t.strip_prefix('p')).ok_or_else(bad)?;
    if t.is_empty() {
        return Err(bad());
    }
    let (mut secs, mut ticks) = (0i64, 0i64);
    let mut in_time = false;
    let mut num = String::new();
    let mut any = false;
    for c in t.chars() {
        match c {
            'T' | 't' => {
                if in_time || !num.is_empty() {
                    return Err(bad());
                }
                in_time = true;
            }
            '0'..='9' | '.' | ',' => num.push(if c == ',' { '.' } else { c }),
            unit => {
                if num.is_empty() {
                    return Err(bad());
                }
                let (whole, frac) = num.split_once('.').unwrap_or((&num, ""));
                let w: i64 = whole.parse().map_err(|_| bad())?;
                let mut f = frac.to_string();
                f.truncate(7);
                while f.len() < 7 {
                    f.push('0');
                }
                let f: i64 = f.parse().map_err(|_| bad())?;
                let (per, frac_ok) = match (unit.to_ascii_uppercase(), in_time) {
                    ('Y', false) => (365 * 86_400, false),
                    ('M', false) => (30 * 86_400, false),
                    ('D', false) => (86_400, false),
                    ('H', true) => (3600, false),
                    ('M', true) => (60, false),
                    ('S', true) => (1, true),
                    _ => return Err(bad()),
                };
                if !frac.is_empty() && !frac_ok {
                    return Err(format!("'{text}': only the seconds of an ISO 8601 duration may have a fraction."));
                }
                secs = w.checked_mul(per).and_then(|x| secs.checked_add(x)).ok_or_else(bad)?;
                ticks = ticks.checked_add(f).ok_or_else(bad)?;
                num.clear();
                any = true;
            }
        }
    }
    if !num.is_empty() || !any {
        return Err(bad());
    }
    secs = secs.checked_add(ticks / TICKS_PER_SEC).ok_or_else(bad)?;
    ticks %= TICKS_PER_SEC;
    Ok(if neg { (-secs, -ticks) } else { (secs, ticks) })
}

/// The text a date has when no format is asked for: `yyyy-MM-ddTHH:mm:ssZ`.
pub fn iso(dt: &Dt) -> String {
    format(dt, "yyyy-MM-ddTHH:mm:ssZ").unwrap_or_default()
}

/// Format with a .NET standard (one character) or custom format string.
pub fn format(dt: &Dt, fmt: &str) -> Result<String, String> {
    let p = dt.parts();
    let chars: Vec<char> = fmt.chars().collect();
    if chars.is_empty() {
        return Err("the date format is empty.".into());
    }
    if chars.len() == 1 {
        let pattern = match chars[0] {
            'd' => "MM/dd/yyyy",
            'D' => "dddd, dd MMMM yyyy",
            'f' => "dddd, dd MMMM yyyy HH:mm",
            'F' => "dddd, dd MMMM yyyy HH:mm:ss",
            'g' => "MM/dd/yyyy HH:mm",
            'G' => "MM/dd/yyyy HH:mm:ss",
            'm' | 'M' => "MMMM dd",
            'o' | 'O' => "yyyy'-'MM'-'dd'T'HH':'mm':'ss'.'fffffffK",
            'r' | 'R' => "ddd, dd MMM yyyy HH':'mm':'ss 'GMT'",
            's' => "yyyy'-'MM'-'dd'T'HH':'mm':'ss",
            't' => "HH:mm",
            'T' => "HH:mm:ss",
            'u' => "yyyy'-'MM'-'dd HH':'mm':'ss'Z'",
            'y' | 'Y' => "yyyy MMMM",
            c => {
                return Err(format!("the standard date format '{c}' is not a .NET date format; use a custom format such as 'yyyy-MM-dd'."))
            }
        };
        return custom(&p, pattern);
    }
    custom(&p, fmt)
}

fn custom(p: &Parts, fmt: &str) -> Result<String, String> {
    let c: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let run = |i: usize, ch: char| c[i..].iter().take_while(|x| **x == ch).count();
    while i < c.len() {
        let ch = c[i];
        match ch {
            'd' => {
                let n = run(i, 'd');
                match n {
                    1 => out.push_str(&p.day.to_string()),
                    2 => out.push_str(&format!("{:02}", p.day)),
                    3 => out.push_str(&DAYS[p.weekday][..3]),
                    4 => out.push_str(DAYS[p.weekday]),
                    _ => return Err("the date format has more than four 'd' in a row.".into()),
                }
                i += n;
            }
            'f' | 'F' => {
                let n = run(i, ch);
                if n > 7 {
                    return Err(format!("the date format has more than seven '{ch}' in a row."));
                }
                let full = format!("{:07}", p.ticks);
                let shown = &full[..n];
                if ch == 'f' {
                    out.push_str(shown);
                } else {
                    let t = shown.trim_end_matches('0');
                    if !t.is_empty() {
                        out.push_str(t);
                    } else if out.ends_with('.') {
                        out.pop();
                    }
                }
                i += n;
            }
            'g' => {
                let n = run(i, 'g');
                if n > 2 {
                    return Err("the date format has more than two 'g' in a row.".into());
                }
                out.push_str("A.D.");
                i += n;
            }
            'h' => {
                let n = run(i, 'h');
                let h = if p.hour % 12 == 0 { 12 } else { p.hour % 12 };
                match n {
                    1 => out.push_str(&h.to_string()),
                    2 => out.push_str(&format!("{h:02}")),
                    _ => return Err("the date format has more than two 'h' in a row.".into()),
                }
                i += n;
            }
            'H' => {
                let n = run(i, 'H');
                match n {
                    1 => out.push_str(&p.hour.to_string()),
                    2 => out.push_str(&format!("{:02}", p.hour)),
                    _ => return Err("the date format has more than two 'H' in a row.".into()),
                }
                i += n;
            }
            'K' => {
                // Azure formats a DateTimeOffset: `K` is the offset, `+00:00`, even for UTC (checked against a real deployment)
                out.push_str("+00:00");
                i += 1;
            }
            'm' => {
                let n = run(i, 'm');
                match n {
                    1 => out.push_str(&p.minute.to_string()),
                    2 => out.push_str(&format!("{:02}", p.minute)),
                    _ => return Err("the date format has more than two 'm' in a row.".into()),
                }
                i += n;
            }
            'M' => {
                let n = run(i, 'M');
                match n {
                    1 => out.push_str(&p.month.to_string()),
                    2 => out.push_str(&format!("{:02}", p.month)),
                    3 => out.push_str(&MONTHS[p.month as usize - 1][..3]),
                    4 => out.push_str(MONTHS[p.month as usize - 1]),
                    _ => return Err("the date format has more than four 'M' in a row.".into()),
                }
                i += n;
            }
            's' => {
                let n = run(i, 's');
                match n {
                    1 => out.push_str(&p.second.to_string()),
                    2 => out.push_str(&format!("{:02}", p.second)),
                    _ => return Err("the date format has more than two 's' in a row.".into()),
                }
                i += n;
            }
            't' => {
                let n = run(i, 't');
                let pm = p.hour >= 12;
                match n {
                    1 => out.push(if pm { 'P' } else { 'A' }),
                    2 => out.push_str(if pm { "PM" } else { "AM" }),
                    _ => return Err("the date format has more than two 't' in a row.".into()),
                }
                i += n;
            }
            'y' => {
                let n = run(i, 'y');
                match n {
                    1 => out.push_str(&(p.year % 100).to_string()),
                    2 => out.push_str(&format!("{:02}", p.year % 100)),
                    3..=5 => out.push_str(&format!("{:0width$}", p.year, width = n)),
                    _ => return Err("the date format has more than five 'y' in a row.".into()),
                }
                i += n;
            }
            'z' => {
                let n = run(i, 'z');
                match n {
                    1 => out.push_str("+0"),
                    2 => out.push_str("+00"),
                    3 => out.push_str("+00:00"),
                    _ => return Err("the date format has more than three 'z' in a row.".into()),
                }
                i += n;
            }
            ':' | '/' => {
                out.push(ch);
                i += 1;
            }
            '\'' | '"' => {
                let mut j = i + 1;
                while j < c.len() && c[j] != ch {
                    out.push(c[j]);
                    j += 1;
                }
                if j >= c.len() {
                    return Err(format!("the date format has a quote ({ch}) that is not closed."));
                }
                i = j + 1;
            }
            '\\' => {
                let next = c.get(i + 1).ok_or("the date format ends with a backslash.")?;
                out.push(*next);
                i += 2;
            }
            '%' => {
                // `%d`: a single custom specifier (so that it is not read as a standard format)
                let next = *c.get(i + 1).ok_or("the date format ends with a percent sign.")?;
                if next == '%' {
                    return Err("the date format has '%%'.".into());
                }
                let spec: String = std::iter::once(next).collect();
                out.push_str(&custom(p, &spec)?);
                i += 2;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
        if out.len() > 4096 {
            return Err("the result of the date format is too long.".into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Dt {
        parse(text).unwrap_or_else(|| panic!("cannot parse {text}"))
    }

    #[test]
    fn parses_the_forms_a_template_uses() {
        // utcNow('u'), the default of utcNow(), an extended form and a zone offset
        assert_eq!(iso(&at("2020-04-07 14:53:14Z")), "2020-04-07T14:53:14Z");
        assert_eq!(iso(&at("20200407T145314Z")), "2020-04-07T14:53:14Z");
        assert_eq!(iso(&at("2023-01-01T00:00:00Z")), "2023-01-01T00:00:00Z");
        assert_eq!(iso(&at("2023-06-01T12:00:00+02:00")), "2023-06-01T10:00:00Z");
        assert_eq!(iso(&parse_wall("2023-06-01T12:00:00+02:00").unwrap()), "2023-06-01T12:00:00Z");
        assert_eq!(iso(&at("2024-02-29")), "2024-02-29T00:00:00Z");
        assert!(parse("2023-02-29").is_none());
        assert!(parse("not a date").is_none());
        assert!(parse("2023-13-01").is_none());
        assert!(parse("2023-01-01T25:00:00Z").is_none());
    }

    #[test]
    fn durations_follow_the_documentation() {
        // Microsoft: P1Y is 365 days and P1M is 30 days; the four examples of the "Remarks" of dateTimeAdd.
        let add = |base: &str, d: &str| {
            let (s, t) = parse_duration(d).unwrap();
            iso(&at(base).add(s, t).unwrap())
        };
        assert_eq!(add("2023-01-01 00:00:00Z", "P1Y"), "2024-01-01T00:00:00Z");
        assert_eq!(add("2024-01-01 00:00:00Z", "P1Y"), "2024-12-31T00:00:00Z");
        assert_eq!(add("2023-02-01 00:00:00Z", "P1M"), "2023-03-03T00:00:00Z");
        assert_eq!(add("2024-02-01 00:00:00Z", "P1M"), "2024-03-02T00:00:00Z");
        // the "Examples" of dateTimeAdd, base 2020-04-07 14:53:14Z
        assert_eq!(add("2020-04-07 14:53:14Z", "P3Y"), "2023-04-07T14:53:14Z");
        assert_eq!(add("2020-04-07 14:53:14Z", "-P9D"), "2020-03-29T14:53:14Z");
        assert_eq!(add("2020-04-07 14:53:14Z", "PT1H"), "2020-04-07T15:53:14Z");
        assert_eq!(add("2020-04-07 14:53:14Z", "PT90M"), "2020-04-07T16:23:14Z");
        assert!(parse_duration("P1W").is_err());
        assert!(parse_duration("P").is_err());
        assert!(parse_duration("1D").is_err());
        assert!(parse_duration("PT1D").is_err());
        assert!(parse_duration("P1.5D").is_err());
        assert!(parse_duration("P1Y2").is_err());
        assert!(at("9999-12-31T00:00:00Z").add(2 * 86_400, 0).is_err());
    }

    #[test]
    fn standard_formats() {
        let d = at("2019-03-05T17:53:18Z");
        // the outputs of the utcNow examples: 'd' is 03/05/2019 and the default is yyyyMMddTHHmmssZ
        assert_eq!(format(&d, "d").unwrap(), "03/05/2019");
        assert_eq!(format(&d, "yyyyMMddTHHmmssZ").unwrap(), "20190305T175318Z");
        assert_eq!(format(&d, "M d").unwrap(), "3 5");
        // the tables of the .NET page, invariant culture
        assert_eq!(format(&d, "u").unwrap(), "2019-03-05 17:53:18Z");
        assert_eq!(format(&d, "s").unwrap(), "2019-03-05T17:53:18");
        assert_eq!(format(&d, "o").unwrap(), "2019-03-05T17:53:18.0000000+00:00");
        assert_eq!(format(&d, "D").unwrap(), "Tuesday, 05 March 2019");
        assert_eq!(format(&d, "f").unwrap(), "Tuesday, 05 March 2019 17:53");
        assert_eq!(format(&d, "F").unwrap(), "Tuesday, 05 March 2019 17:53:18");
        assert_eq!(format(&d, "g").unwrap(), "03/05/2019 17:53");
        assert_eq!(format(&d, "G").unwrap(), "03/05/2019 17:53:18");
        assert_eq!(format(&d, "m").unwrap(), "March 05");
        assert_eq!(format(&d, "r").unwrap(), "Tue, 05 Mar 2019 17:53:18 GMT");
        assert_eq!(format(&d, "t").unwrap(), "17:53");
        assert_eq!(format(&d, "T").unwrap(), "17:53:18");
        assert!(format(&d, "U").is_err()); // Azure: "not a valid DateTime format string"
        assert_eq!(format(&d, "y").unwrap(), "2019 March");
    }

    #[test]
    fn custom_formats() {
        let d = at("2008-06-15T13:45:30.1234567Z");
        // the examples of the custom format page
        assert_eq!(format(&d, "yyyy-MM-ddTHH:mm:ss.fffZ").unwrap(), "2008-06-15T13:45:30.123Z");
        assert_eq!(format(&d, "hh:mm:ss tt").unwrap(), "01:45:30 PM");
        assert_eq!(format(&d, "h:m:s t").unwrap(), "1:45:30 P");
        assert_eq!(format(&d, "dddd d MMMM yy").unwrap(), "Sunday 15 June 08");
        assert_eq!(format(&d, "ddd MMM").unwrap(), "Sun Jun");
        assert_eq!(format(&d, "ffffff").unwrap(), "123456");
        assert_eq!(format(&d, "FFFFFFF").unwrap(), "1234567");
        assert_eq!(format(&d, "s.FFF").unwrap(), "30.123");
        assert_eq!(format(&at("2008-06-15T13:45:30Z"), "s.FFF").unwrap(), "30");
        assert_eq!(format(&d, "%d").unwrap(), "15");
        assert_eq!(format(&d, "'year' yyyy").unwrap(), "year 2008");
        assert_eq!(format(&d, "yyyy\\-MM").unwrap(), "2008-06");
        assert_eq!(format(&d, "K zzz z zz").unwrap(), "+00:00 +00:00 +0 +00");
        assert_eq!(format(&at("0099-01-01T00:00:00Z"), "yyyy yyy yy y").unwrap(), "0099 099 99 99");
        assert_eq!(format(&at("2008-06-15T00:45:30Z"), "h tt").unwrap(), "12 AM");
    }

    #[test]
    fn unknown_formats_are_errors_not_wrong_dates() {
        let d = at("2019-03-05T17:53:18Z");
        assert!(format(&d, "a").unwrap_err().contains("'a'"));
        assert!(format(&d, "").is_err());
        assert!(format(&d, "ddddd").unwrap_err().contains("'d'"));
        assert!(format(&d, "MMMMM").unwrap_err().contains("'M'"));
        assert!(format(&d, "ffffffff").unwrap_err().contains("'f'"));
        assert!(format(&d, "yyyyyy").unwrap_err().contains("'y'"));
        assert!(format(&d, "'open").unwrap_err().contains("not closed"));
        assert!(format(&d, "HHH").is_err());
    }

    #[test]
    fn epoch_round_trip() {
        // 2023-05-02T15:16:13Z is 1683040573 (dateTimeFromEpoch / dateTimeToEpoch examples)
        assert_eq!(at("2023-05-02T15:16:13Z").secs, 1_683_040_573);
        assert_eq!(iso(&Dt::from_epoch(1_683_040_573).unwrap()), "2023-05-02T15:16:13Z");
        assert_eq!(iso(&Dt::from_epoch(0).unwrap()), "1970-01-01T00:00:00Z");
        assert_eq!(iso(&Dt::from_epoch(-1).unwrap()), "1969-12-31T23:59:59Z");
        assert!(Dt::from_epoch(i64::MAX).is_err());
    }

    #[test]
    fn a_duration_that_adds_up_to_more_than_a_number_can_hold_is_refused() {
        for text in [
            "PT9223372036854775807S0.9999999S0.9999999S",
            "PT9223372036854775807S0.9999999S",
            "PT9223372036854775807S",
            "-PT9223372036854775807S0.9999999S0.9999999S",
            "P9223372036854775807D",
        ] {
            // an answer, not a panic: refused, or a value that fits
            let _ = parse_duration(text);
        }
        assert!(parse_duration("PT9223372036854775807S0.9999999S0.9999999S").is_err());
        assert_eq!(parse_duration("PT1.5S").unwrap(), (1, 5_000_000));
        assert_eq!(parse_duration("PT0.9999999S0.9999999S").unwrap(), (1, 9_999_998));
    }
}
