//! Calendar math and ISO 8601 parsing/formatting for the date/time functions.
//! Time is kept as signed 100-nanosecond ticks since the Unix epoch.

pub(crate) const TICKS_PER_SEC: i64 = 10_000_000;
pub(crate) const TICKS_PER_DAY: i64 = 86_400 * TICKS_PER_SEC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
}

impl Part {
    pub fn parse(s: &str) -> Option<Part> {
        Some(match s.to_ascii_lowercase().as_str() {
            "year" | "yyyy" | "yy" => Part::Year,
            "month" | "mm" | "m" => Part::Month,
            "day" | "dd" | "d" => Part::Day,
            "hour" | "hh" => Part::Hour,
            "minute" | "mi" | "n" => Part::Minute,
            "second" | "ss" | "s" => Part::Second,
            "millisecond" | "ms" => Part::Millisecond,
            "microsecond" | "mcs" => Part::Microsecond,
            "nanosecond" | "ns" => Part::Nanosecond,
            _ => return None,
        })
    }
    /// Length of the part in nanoseconds (not for year/month).
    pub fn nanos(self) -> Option<i128> {
        Some(match self {
            Part::Day => 86_400_000_000_000,
            Part::Hour => 3_600_000_000_000,
            Part::Minute => 60_000_000_000,
            Part::Second => 1_000_000_000,
            Part::Millisecond => 1_000_000,
            Part::Microsecond => 1_000,
            Part::Nanosecond => 1,
            _ => return None,
        })
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Dt {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 100 ns units, 0..10_000_000
    pub frac: u32,
}

impl Dt {
    pub fn new(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32, frac: u32) -> Option<Dt> {
        if !(1..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
            || frac >= 10_000_000
        {
            return None;
        }
        Some(Dt { year, month, day, hour, minute, second, frac })
    }

    pub fn ticks(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * TICKS_PER_DAY
            + (self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64) * TICKS_PER_SEC
            + self.frac as i64
    }

    pub fn from_ticks(t: i64) -> Option<Dt> {
        let days = t.div_euclid(TICKS_PER_DAY);
        let rem = t.rem_euclid(TICKS_PER_DAY);
        let (y, m, d) = civil_from_days(days);
        let secs = rem / TICKS_PER_SEC;
        Dt::new(y, m, d, (secs / 3600) as u32, ((secs / 60) % 60) as u32, (secs % 60) as u32, (rem % TICKS_PER_SEC) as u32)
    }

    /// `yyyy-MM-ddTHH:mm:ss.fffffffZ`
    pub fn format(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.frac
        )
    }

    /// Adds years/months with day clamping.
    pub fn add_months(&self, months: i64) -> Option<Dt> {
        let total = self.year.checked_mul(12)?.checked_add(self.month as i64 - 1)?.checked_add(months)?;
        let y = total.div_euclid(12);
        let m = (total.rem_euclid(12) + 1) as u32;
        if !(1..=9999).contains(&y) {
            return None;
        }
        let d = self.day.min(days_in_month(y, m));
        Dt::new(y, m, d, self.hour, self.minute, self.second, self.frac)
    }
}

fn digits(c: &[char], at: usize, n: usize) -> Option<u32> {
    if at + n > c.len() {
        return None;
    }
    let mut v = 0u32;
    for ch in &c[at..at + n] {
        v = v * 10 + ch.to_digit(10).filter(|_| ch.is_ascii_digit())?;
    }
    Some(v)
}

/// Parses `YYYY-MM-DD[Z]` or `YYYY-MM-DD(T| )hh:mm:ss[.f{1,7}][Z]`.
pub(crate) fn parse_datetime(s: &str) -> Option<Dt> {
    let c: Vec<char> = s.chars().collect();
    let year = digits(&c, 0, 4)? as i64;
    if c.get(4) != Some(&'-') {
        return None;
    }
    let month = digits(&c, 5, 2)?;
    if c.get(7) != Some(&'-') {
        return None;
    }
    let day = digits(&c, 8, 2)?;
    let mut i = 10;
    if i == c.len() || (i + 1 == c.len() && c[i] == 'Z') {
        return Dt::new(year, month, day, 0, 0, 0, 0);
    }
    if c[i] != 'T' && c[i] != ' ' {
        return None;
    }
    i += 1;
    let hour = digits(&c, i, 2)?;
    if c.get(i + 2) != Some(&':') {
        return None;
    }
    let minute = digits(&c, i + 3, 2)?;
    if c.get(i + 5) != Some(&':') {
        return None;
    }
    let second = digits(&c, i + 6, 2)?;
    i += 8;
    let mut frac = 0u32;
    if c.get(i) == Some(&'.') {
        i += 1;
        let start = i;
        while i < c.len() && c[i].is_ascii_digit() {
            i += 1;
        }
        let n = i - start;
        if n == 0 || n > 7 {
            return None;
        }
        let mut f = 0u32;
        for ch in &c[start..i] {
            f = f * 10 + ch.to_digit(10)?;
        }
        frac = f * 10u32.pow(7 - n as u32);
    }
    if i < c.len() {
        if c[i] == 'Z' && i + 1 == c.len() {
        } else {
            return None;
        }
    }
    Dt::new(year, month, day, hour, minute, second, frac)
}

/// Formats with `DATETIMEFORMAT` specifiers.
pub(crate) fn format_custom(dt: &Dt, fmt: &str) -> String {
    let f: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < f.len() {
        let c = f[i];
        if c == '\\' {
            if i + 1 < f.len() {
                out.push(f[i + 1]);
            }
            i += 2;
            continue;
        }
        let mut run = 1;
        while i + run < f.len() && f[i + run] == c {
            run += 1;
        }
        let h12 = if dt.hour.is_multiple_of(12) { 12 } else { dt.hour % 12 };
        let fr = format!("{:07}", dt.frac);
        let produced: Option<(String, usize)> = match c {
            'd' => Some((if run == 1 { dt.day.to_string() } else { format!("{:02}", dt.day) }, run.min(2))),
            'M' => Some((if run == 1 { dt.month.to_string() } else { format!("{:02}", dt.month) }, run.min(2))),
            'y' => {
                let n = run.min(4);
                let s = if n == 1 {
                    (dt.year % 100).to_string()
                } else if n == 2 || n == 3 {
                    format!("{:02}", dt.year % 100)
                } else {
                    format!("{:04}", dt.year)
                };
                Some((s, n))
            }
            'h' => Some((if run == 1 { h12.to_string() } else { format!("{:02}", h12) }, run.min(2))),
            'H' => Some((if run == 1 { dt.hour.to_string() } else { format!("{:02}", dt.hour) }, run.min(2))),
            'm' => Some((if run == 1 { dt.minute.to_string() } else { format!("{:02}", dt.minute) }, run.min(2))),
            's' => Some((if run == 1 { dt.second.to_string() } else { format!("{:02}", dt.second) }, run.min(2))),
            'f' => {
                let n = run.min(7);
                Some((fr[..n].to_string(), n))
            }
            'F' => {
                let n = run.min(7);
                let s = &fr[..n];
                Some((if s.chars().all(|ch| ch == '0') { String::new() } else { s.to_string() }, n))
            }
            't' if run >= 2 => Some((if dt.hour < 12 { "AM".into() } else { "PM".into() }, 2)),
            _ => None,
        };
        match produced {
            Some((s, used)) => {
                out.push_str(&s);
                i += used;
            }
            None => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}
