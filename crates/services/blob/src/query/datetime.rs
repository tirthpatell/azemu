//! Timestamps for the query language: parsing (ISO 8601 as the W3C note describes it), printing, `DATE_ADD`,
//! `DATE_DIFF`, `EXTRACT` and the `TO_STRING` patterns. No time zone database: a timestamp carries the
//! offset it was written with.

use std::cmp::Ordering;

/// A point in time and the offset (minutes east of UTC) it was written with.
#[derive(Debug, Clone, Copy)]
pub struct Ts {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub secs: i64,
    pub nanos: u32,
    pub offset_min: i32,
}

impl PartialEq for Ts {
    fn eq(&self, o: &Ts) -> bool {
        self.secs == o.secs && self.nanos == o.nanos
    }
}

impl PartialOrd for Ts {
    fn partial_cmp(&self, o: &Ts) -> Option<Ordering> {
        Some((self.secs, self.nanos).cmp(&(o.secs, o.nanos)))
    }
}

/// Year, month, day, hour, minute, second of the local time (offset applied).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanos: u32,
    pub offset_min: i32,
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
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

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Ts {
    pub fn from_civil(c: &Civil) -> Ts {
        let days = days_from_civil(c.year, c.month, c.day);
        let local = days * 86_400 + i64::from(c.hour) * 3600 + i64::from(c.minute) * 60 + i64::from(c.second);
        Ts { secs: local - i64::from(c.offset_min) * 60, nanos: c.nanos, offset_min: c.offset_min }
    }

    pub fn civil(&self) -> Civil {
        let local = self.secs + i64::from(self.offset_min) * 60;
        let days = local.div_euclid(86_400);
        let rem = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Civil {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: ((rem % 3600) / 60) as u32,
            second: (rem % 60) as u32,
            nanos: self.nanos,
            offset_min: self.offset_min,
        }
    }

    pub fn utc_now() -> Ts {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Ts { secs: d.as_secs() as i64, nanos: d.subsec_nanos(), offset_min: 0 }
    }
}

fn digits(s: &[u8], i: &mut usize, min: usize, max: usize) -> Option<u32> {
    let st = *i;
    let mut n = 0u32;
    while *i < s.len() && *i - st < max && s[*i].is_ascii_digit() {
        n = n * 10 + u32::from(s[*i] - b'0');
        *i += 1;
    }
    if *i - st >= min {
        Some(n)
    } else {
        None
    }
}

/// ISO 8601 as the W3C note allows: `2007`, `2007-02`, `2007-02-23`, each with an optional `T`, then
/// `hh:mm`, `hh:mm:ss` or `hh:mm:ss.fraction`, then `Z` or `+hh:mm` / `-hh:mm`. Missing parts are the
/// first of their kind (month 1, day 1, time 00:00:00), no zone is UTC. `None` when the text is not one of them.
pub fn parse(text: &str) -> Option<Ts> {
    let s = text.trim().as_bytes();
    let mut i = 0;
    let year = digits(s, &mut i, 4, 4)?;
    let (mut month, mut day) = (1, 1);
    let (mut hour, mut minute, mut second, mut nanos) = (0, 0, 0, 0u32);
    let mut offset = 0i32;
    if i < s.len() && s[i] == b'-' {
        i += 1;
        month = digits(s, &mut i, 2, 2)?;
        if i < s.len() && s[i] == b'-' {
            i += 1;
            day = digits(s, &mut i, 2, 2)?;
        }
    }
    if i < s.len() && s[i] == b'T' {
        i += 1;
        if i < s.len() {
            hour = digits(s, &mut i, 2, 2)?;
            if i >= s.len() || s[i] != b':' {
                return None;
            }
            i += 1;
            minute = digits(s, &mut i, 2, 2)?;
            if i < s.len() && s[i] == b':' {
                i += 1;
                second = digits(s, &mut i, 2, 2)?;
                if i < s.len() && s[i] == b'.' {
                    i += 1;
                    let st = i;
                    let mut frac = 0u64;
                    while i < s.len() && s[i].is_ascii_digit() {
                        if i - st < 9 {
                            frac = frac * 10 + u64::from(s[i] - b'0');
                        }
                        i += 1;
                    }
                    let n = (i - st).min(9);
                    if i == st {
                        return None;
                    }
                    nanos = (frac * 10u64.pow((9 - n) as u32)) as u32;
                }
            }
            if i < s.len() {
                match s[i] {
                    b'Z' => i += 1,
                    b'+' | b'-' => {
                        let sign = if s[i] == b'-' { -1 } else { 1 };
                        i += 1;
                        let h = digits(s, &mut i, 2, 2)?;
                        if i < s.len() && s[i] == b':' {
                            i += 1;
                        }
                        let m = digits(s, &mut i, 2, 2)?;
                        if h > 23 || m > 59 {
                            return None;
                        }
                        offset = sign * (h * 60 + m) as i32;
                    }
                    _ => return None,
                }
            }
        }
    }
    if i != s.len() {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(i64::from(year), month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(Ts::from_civil(&Civil { year: i64::from(year), month, day, hour, minute, second, nanos, offset_min: offset }))
}

/// `2007-02-23T12:14:33.079Z`: the fraction only when there is one (trailing zeros dropped), `Z` for UTC.
pub fn format(t: &Ts) -> String {
    let c = t.civil();
    let mut s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", c.year, c.month, c.day, c.hour, c.minute, c.second);
    if c.nanos != 0 {
        let f = format!("{:09}", c.nanos);
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    if c.offset_min == 0 {
        s.push('Z');
    } else {
        let a = c.offset_min.abs();
        s.push_str(&format!("{}{:02}:{:02}", if c.offset_min < 0 { '-' } else { '+' }, a / 60, a % 60));
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    TimezoneHour,
    TimezoneMinute,
}

pub fn part(name: &str) -> Option<Part> {
    Some(match name.to_ascii_lowercase().as_str() {
        "year" => Part::Year,
        "month" => Part::Month,
        "day" => Part::Day,
        "hour" => Part::Hour,
        "minute" => Part::Minute,
        "second" => Part::Second,
        "timezone_hour" => Part::TimezoneHour,
        "timezone_minute" => Part::TimezoneMinute,
        _ => return None,
    })
}

pub fn extract(p: Part, t: &Ts) -> i64 {
    let c = t.civil();
    match p {
        Part::Year => c.year,
        Part::Month => i64::from(c.month),
        Part::Day => i64::from(c.day),
        Part::Hour => i64::from(c.hour),
        Part::Minute => i64::from(c.minute),
        Part::Second => i64::from(c.second),
        Part::TimezoneHour => i64::from(c.offset_min / 60),
        Part::TimezoneMinute => i64::from(c.offset_min % 60),
    }
}

/// `DATE_ADD(part, n, t)`; `None` when the result is out of range.
pub fn add(p: Part, n: i64, t: &Ts) -> Option<Ts> {
    match p {
        Part::Year | Part::Month => {
            let c = t.civil();
            let months = if p == Part::Year { n.checked_mul(12)? } else { n };
            let total = c.year.checked_mul(12)?.checked_add(i64::from(c.month) - 1)?.checked_add(months)?;
            let year = total.div_euclid(12);
            let month = total.rem_euclid(12) as u32 + 1;
            if !(0..=9999).contains(&year) {
                return None;
            }
            let day = c.day.min(days_in_month(year, month));
            Some(Ts::from_civil(&Civil { year, month, day, ..c }))
        }
        Part::Day | Part::Hour | Part::Minute | Part::Second => {
            let unit = match p {
                Part::Day => 86_400,
                Part::Hour => 3600,
                Part::Minute => 60,
                _ => 1,
            };
            let secs = t.secs.checked_add(n.checked_mul(unit)?)?;
            if !(-62_135_596_800..=253_402_300_799).contains(&secs) {
                return None;
            }
            Some(Ts { secs, ..*t })
        }
        Part::TimezoneHour | Part::TimezoneMinute => None,
    }
}

/// `DATE_DIFF(part, a, b)`: whole units from `a` to `b`, negative when `b` is earlier.
pub fn diff(p: Part, a: &Ts, b: &Ts) -> Option<i64> {
    match p {
        Part::Year | Part::Month => {
            let (x, y, sign) = if b >= a { (a, b, 1) } else { (b, a, -1) };
            let (cx, cy) = (x.civil(), y.civil());
            let mut months = (cy.year - cx.year) * 12 + i64::from(cy.month) - i64::from(cx.month);
            // not a whole month yet when the rest of the month is smaller
            let rest = |c: &Civil| (c.day, c.hour, c.minute, c.second, c.nanos);
            if rest(&cy) < rest(&cx) {
                months -= 1;
            }
            Some(sign * if p == Part::Year { months / 12 } else { months })
        }
        Part::Day | Part::Hour | Part::Minute | Part::Second => {
            let unit = match p {
                Part::Day => 86_400,
                Part::Hour => 3600,
                Part::Minute => 60,
                _ => 1,
            };
            let mut d = i128::from(b.secs - a.secs);
            let dn = i64::from(b.nanos) - i64::from(a.nanos);
            // whole units, truncated toward zero, counting the fraction of a second
            if d > 0 && dn < 0 {
                d -= 1;
            } else if d < 0 && dn > 0 {
                d += 1;
            }
            i64::try_from(d / i128::from(unit)).ok()
        }
        Part::TimezoneHour | Part::TimezoneMinute => None,
    }
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// `TO_STRING(t, pattern)`. Letters not in the table below are an error (`Err` names the letter);
/// text between single quotes is copied, other characters are copied as they are.
pub fn to_string(t: &Ts, pattern: &str) -> Result<String, String> {
    let c = t.civil();
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                out.push(chars[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        if !ch.is_ascii_alphabetic() {
            out.push(ch);
            i += 1;
            continue;
        }
        let mut n = 1;
        while i + n < chars.len() && chars[i + n] == ch {
            n += 1;
        }
        i += n;
        let h12 = if c.hour.is_multiple_of(12) { 12 } else { c.hour % 12 };
        match (ch, n) {
            ('y', 2) => out.push_str(&format!("{:02}", c.year.rem_euclid(100))),
            ('y', _) => out.push_str(&format!("{:04}", c.year)),
            ('M', 1) => out.push_str(&c.month.to_string()),
            ('M', 2) => out.push_str(&format!("{:02}", c.month)),
            ('M', 3) => out.push_str(&MONTHS[c.month as usize - 1][..3].to_ascii_uppercase()),
            ('M', 4) => out.push_str(MONTHS[c.month as usize - 1]),
            ('M', 5) => out.push_str(&MONTHS[c.month as usize - 1][..1]),
            ('d', 1) => out.push_str(&c.day.to_string()),
            ('d', 2) => out.push_str(&format!("{:02}", c.day)),
            ('a', _) => out.push_str(if c.hour < 12 { "AM" } else { "PM" }),
            ('h', 1) => out.push_str(&h12.to_string()),
            ('h', 2) => out.push_str(&format!("{h12:02}")),
            ('H', 1) => out.push_str(&c.hour.to_string()),
            ('H', 2) => out.push_str(&format!("{:02}", c.hour)),
            ('m', 1) => out.push_str(&c.minute.to_string()),
            ('m', 2) => out.push_str(&format!("{:02}", c.minute)),
            ('s', 1) => out.push_str(&c.second.to_string()),
            ('s', 2) => out.push_str(&format!("{:02}", c.second)),
            ('S', 1..=9) => out.push_str(&format!("{:09}", c.nanos)[..n]),
            ('X' | 'x', _) => {
                let a = c.offset_min.abs();
                let sign = if c.offset_min < 0 { '-' } else { '+' };
                if ch == 'X' && c.offset_min == 0 {
                    out.push('Z');
                } else {
                    match n {
                        1 => out.push_str(&format!("{sign}{:02}", a / 60)),
                        2 | 4 => out.push_str(&format!("{sign}{:02}{:02}", a / 60, a % 60)),
                        _ => out.push_str(&format!("{sign}{:02}:{:02}", a / 60, a % 60)),
                    }
                }
            }
            _ => return Err(format!("the pattern letters {} are not supported", ch.to_string().repeat(n))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Ts {
        parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn days_and_dates_agree_with_known_values() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(p("2018-11-09T00:00Z").secs, 1_541_721_600);
        assert_eq!(p("1969-07-20T20:18Z").secs, -14_182_920);
    }

    #[test]
    fn iso_8601_forms() {
        assert_eq!(format(&p("2007T")), "2007-01-01T00:00:00Z");
        assert_eq!(format(&p("2007-02T")), "2007-02-01T00:00:00Z");
        assert_eq!(format(&p("2007-02-23")), "2007-02-23T00:00:00Z");
        assert_eq!(format(&p("2007-02-23T12:14Z")), "2007-02-23T12:14:00Z");
        assert_eq!(format(&p("2007-02-23T12:14:33.079Z")), "2007-02-23T12:14:33.079Z");
        assert_eq!(format(&p("2018-11-09T00:00+05:30")), "2018-11-09T00:00:00+05:30");
        assert_eq!(p("2018-11-09T00:00+05:30").secs, p("2018-11-08T18:30Z").secs);
        for bad in ["", "abc", "2007-13", "2007-02-30", "2007-02-23T25:00", "2007-02-23T12", "2007-02-23T12:14X", "20070223"] {
            assert!(parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn documented_examples() {
        // DATE_ADD('minute', 1, '2017-01-02T03:04:05.006Z')
        assert_eq!(format(&add(Part::Minute, 1, &p("2017-01-02T03:04:05.006Z")).unwrap()), "2017-01-02T03:05:05.006Z");
        // DATE_DIFF('hour','2018-11-09T00:00+05:30','2018-11-09T01:00:23-08:00'): 00:00+05:30 is 18:30Z the day before,
        // 01:00:23-08:00 is 09:00:23Z: 14 hours and a bit
        assert_eq!(diff(Part::Hour, &p("2018-11-09T00:00+05:30"), &p("2018-11-09T01:00:23-08:00")), Some(14));
        // TO_STRING(CAST('1969-07-20T20:18Z' AS TIMESTAMP), 'MMMM d, y')
        assert_eq!(to_string(&p("1969-07-20T20:18Z"), "MMMM d, y").unwrap(), "July 20, 1969");
        assert_eq!(extract(Part::Year, &p("2010-01-01T")), 2010);
    }

    #[test]
    fn month_arithmetic_clamps_and_counts_whole_months() {
        assert_eq!(format(&add(Part::Month, 1, &p("2020-01-31")).unwrap()), "2020-02-29T00:00:00Z");
        assert_eq!(format(&add(Part::Year, -1, &p("2020-02-29")).unwrap()), "2019-02-28T00:00:00Z");
        assert_eq!(diff(Part::Month, &p("2020-01-31"), &p("2020-02-29")), Some(0));
        assert_eq!(diff(Part::Month, &p("2020-01-15"), &p("2020-03-15")), Some(2));
        assert_eq!(diff(Part::Year, &p("2000-06-01"), &p("2005-05-31")), Some(4));
        assert_eq!(diff(Part::Year, &p("2005-05-31"), &p("2000-06-01")), Some(-4));
        assert_eq!(diff(Part::Day, &p("2020-01-01"), &p("2020-01-03T23:00Z")), Some(2));
        assert_eq!(diff(Part::Second, &p("2020-01-01T00:00:01Z"), &p("2020-01-01T00:00:00Z")), Some(-1));
    }

    #[test]
    fn to_string_patterns() {
        let t = p("2019-03-07T15:04:09.123456Z");
        assert_eq!(to_string(&t, "yyyy-MM-dd HH:mm:ss").unwrap(), "2019-03-07 15:04:09");
        assert_eq!(to_string(&t, "yy M d h a").unwrap(), "19 3 7 3 PM");
        assert_eq!(to_string(&t, "MMM MMMM").unwrap(), "MAR March");
        assert_eq!(to_string(&t, "SSS").unwrap(), "123");
        assert_eq!(to_string(&p("2019-03-07T15:04:09+05:30"), "XXX x").unwrap(), "+05:30 +05");
        assert_eq!(to_string(&t, "X").unwrap(), "Z");
        assert!(to_string(&t, "Q").is_err());
    }
}
