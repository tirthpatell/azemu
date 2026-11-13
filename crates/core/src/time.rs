//! Time formatting and parsing the way Azure writes it.
//!
//! This module also re-exports everything from the `time` crate, so
//! `azemu_core::time::OffsetDateTime` is the crate's type.

pub use ::time::*;

use ::time as tm;

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `Thu, 27 Oct 2022 22:06:34 GMT`
pub fn rfc1123(t: tm::OffsetDateTime) -> String {
    let t = t.to_offset(tm::UtcOffset::UTC);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[t.weekday().number_days_from_monday() as usize],
        t.day(),
        MONTHS[t.month() as usize - 1],
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// `2022-10-27T22:06:34.6827829Z` (always 7 fraction digits).
pub fn iso8601_7(t: tm::OffsetDateTime) -> String {
    let t = t.to_offset(tm::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 100
    )
}

/// `2022-10-27T22:06:34Z`
pub fn iso8601_secs(t: tm::OffsetDateTime) -> String {
    let t = t.to_offset(tm::UtcOffset::UTC);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

/// Parse an RFC 1123 date (`Thu, 27 Oct 2022 22:06:34 GMT`). The weekday is not
/// checked. `GMT`, `UTC`, `UT`, `Z` and numeric offsets are accepted.
pub fn parse_rfc1123(s: &str) -> Option<tm::OffsetDateTime> {
    let s = s.trim();
    let s = match s.split_once(',') {
        Some((day, rest)) if day.trim().len() >= 3 => rest.trim(),
        Some(_) => return None,
        None => s,
    };
    let mut it = s.split_whitespace();
    let day: u8 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(mon.get(..3).unwrap_or("")))? as u8 + 1;
    let year: i32 = it.next()?.parse().ok()?;
    let year = if year < 100 {
        if year < 70 {
            year + 2000
        } else {
            year + 1900
        }
    } else {
        year
    };
    let (h, m, sec) = parse_hms(it.next()?)?;
    let offset = match it.next() {
        None => tm::UtcOffset::UTC,
        Some(z) => parse_zone(z)?,
    };
    if it.next().is_some() {
        return None;
    }
    build(year, month, day, h, m, sec, 0, offset)
}

/// Parse an ISO 8601 / RFC 3339 timestamp.
///
/// Accepts `2022-10-27`, `2022-10-27T22:06Z`, `2022-10-27T22:06:34Z`, any number
/// of fraction digits (digits beyond nanoseconds are dropped), and `Z`, `+hh:mm`,
/// `+hhmm` or `+hh` offsets. No zone means UTC. The result is converted to UTC.
pub fn parse_iso8601(s: &str) -> Option<tm::OffsetDateTime> {
    let s = s.trim();
    if !s.is_ascii() || s.len() < 10 {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    if s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        return None;
    }
    let month: u8 = s.get(5..7)?.parse().ok()?;
    let day: u8 = s.get(8..10)?.parse().ok()?;
    let rest = &s[10..];
    if rest.is_empty() {
        return build(year, month, day, 0, 0, 0, 0, tm::UtcOffset::UTC);
    }
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix('t')).or_else(|| rest.strip_prefix(' '))?;
    // split time from zone
    let zone_at = rest.find(['Z', 'z', '+', '-']);
    let (time_part, zone) = match zone_at {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (hms, frac) = match time_part.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (time_part, None),
    };
    let (h, m, sec) = parse_hms(hms)?;
    let nanos = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut digits: String = f.chars().take(9).collect();
            while digits.len() < 9 {
                digits.push('0');
            }
            digits.parse().ok()?
        }
    };
    let offset = if zone.is_empty() { tm::UtcOffset::UTC } else { parse_zone(zone)? };
    build(year, month, day, h, m, sec, nanos, offset)
}

fn parse_hms(s: &str) -> Option<(u8, u8, u8)> {
    let mut p = s.split(':');
    let h: u8 = p.next()?.parse().ok()?;
    let m: u8 = p.next()?.parse().ok()?;
    let sec: u8 = match p.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if p.next().is_some() {
        return None;
    }
    Some((h, m, sec))
}

fn parse_zone(z: &str) -> Option<tm::UtcOffset> {
    match z {
        "Z" | "z" | "GMT" | "UTC" | "UT" => return Some(tm::UtcOffset::UTC),
        _ => {}
    }
    let (sign, digits) = match z.as_bytes().first()? {
        b'+' => (1i8, &z[1..]),
        b'-' => (-1i8, &z[1..]),
        _ => return None,
    };
    let digits: String = digits.chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i8>().ok()?, 0i8),
        4 => (digits[..2].parse::<i8>().ok()?, digits[2..].parse::<i8>().ok()?),
        _ => return None,
    };
    tm::UtcOffset::from_hms(sign * h, sign * m, 0).ok()
}

#[allow(clippy::too_many_arguments)]
fn build(year: i32, month: u8, day: u8, h: u8, m: u8, s: u8, nanos: u32, offset: tm::UtcOffset) -> Option<tm::OffsetDateTime> {
    let date = tm::Date::from_calendar_date(year, tm::Month::try_from(month).ok()?, day).ok()?;
    // Allow a leap second "60" by clamping, as RFC 3339 permits it.
    let time = tm::Time::from_hms_nano(h, m, s.min(59), nanos).ok()?;
    Some(tm::PrimitiveDateTime::new(date, time).assume_offset(offset).to_offset(tm::UtcOffset::UTC))
}

/// Drop sub-second precision.
pub fn truncate_to_secs(t: tm::OffsetDateTime) -> tm::OffsetDateTime {
    t.replace_nanosecond(0).unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::time::macros::datetime;
    use ::time::OffsetDateTime;

    const T: OffsetDateTime = datetime!(2022-10-27 22:06:34.6827829 UTC);

    #[test]
    fn formats() {
        assert_eq!(rfc1123(T), "Thu, 27 Oct 2022 22:06:34 GMT");
        assert_eq!(iso8601_7(T), "2022-10-27T22:06:34.6827829Z");
        assert_eq!(iso8601_secs(T), "2022-10-27T22:06:34Z");
        assert_eq!(iso8601_7(datetime!(2022-01-02 03:04:05 UTC)), "2022-01-02T03:04:05.0000000Z");
        assert_eq!(iso8601_7(datetime!(2022-01-02 03:04:05.000000123 UTC)), "2022-01-02T03:04:05.0000001Z");
        // offsets are converted to UTC
        assert_eq!(rfc1123(datetime!(2022-10-27 23:06:34 +1)), "Thu, 27 Oct 2022 22:06:34 GMT");
        assert_eq!(rfc1123(datetime!(2022-03-06 00:00:00 UTC)), "Sun, 06 Mar 2022 00:00:00 GMT");
    }

    #[test]
    fn parse_iso_variants() {
        let exp = datetime!(2022-10-27 22:06:34 UTC);
        assert_eq!(parse_iso8601("2022-10-27T22:06:34Z"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T22:06:34"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T22:06:34.0Z"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T22:06:34.6827829Z"), Some(datetime!(2022-10-27 22:06:34.6827829 UTC)));
        assert_eq!(parse_iso8601("2022-10-27T22:06:34.68Z"), Some(datetime!(2022-10-27 22:06:34.68 UTC)));
        assert_eq!(parse_iso8601("2022-10-27T22:06:34.123456789012Z"), Some(datetime!(2022-10-27 22:06:34.123456789 UTC)));
        assert_eq!(parse_iso8601("2022-10-27T23:06:34+01:00"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T23:06:34+0100"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T23:06:34+01"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T16:36:34.000-05:30"), Some(exp));
        assert_eq!(parse_iso8601("2022-10-27T22:06Z"), Some(datetime!(2022-10-27 22:06 UTC)));
        assert_eq!(parse_iso8601("2022-10-27"), Some(datetime!(2022-10-27 00:00 UTC)));
        assert_eq!(parse_iso8601(" 2022-10-27T22:06:34Z "), Some(exp));
    }

    #[test]
    fn parse_iso_rejects_garbage() {
        for bad in [
            "",
            "garbage",
            "2022-13-01T00:00:00Z",
            "2022-02-30T00:00:00Z",
            "2022-10-27T25:00:00Z",
            "2022-10-27T22:06:34.Z",
            "2022-10-27X22:06:34Z",
            "2022-10-27T22:06:34+9",
            "2022/10/27",
            "2022-10-27T22:06:34Zjunk",
            "é022-10-27",
        ] {
            assert_eq!(parse_iso8601(bad), None, "{bad}");
        }
    }

    #[test]
    fn parse_rfc1123_forms() {
        let exp = datetime!(2022-10-27 22:06:34 UTC);
        assert_eq!(parse_rfc1123("Thu, 27 Oct 2022 22:06:34 GMT"), Some(exp));
        assert_eq!(parse_rfc1123("Thu, 27 Oct 2022 22:06:34 +0000"), Some(exp));
        assert_eq!(parse_rfc1123("Thu, 27 Oct 2022 23:06:34 +0100"), Some(exp));
        assert_eq!(parse_rfc1123("27 Oct 2022 22:06:34 GMT"), Some(exp));
        assert_eq!(parse_rfc1123("thu, 27 oct 2022 22:06:34 gmt"), None); // zone is case-sensitive
        assert_eq!(parse_rfc1123("Thu, 27 oct 2022 22:06:34 GMT"), Some(exp));
        assert_eq!(parse_rfc1123("Thu, 27 Foo 2022 22:06:34 GMT"), None);
        assert_eq!(parse_rfc1123("Thu, 32 Oct 2022 22:06:34 GMT"), None);
        assert_eq!(parse_rfc1123("nonsense"), None);
        assert_eq!(parse_rfc1123(""), None);
    }

    #[test]
    fn roundtrips() {
        let t = datetime!(2024-02-29 12:34:56 UTC);
        assert_eq!(parse_rfc1123(&rfc1123(t)), Some(t));
        assert_eq!(parse_iso8601(&iso8601_secs(t)), Some(t));
        assert_eq!(parse_iso8601(&iso8601_7(T)), Some(T));
    }

    #[test]
    fn reexports_time_crate() {
        let _: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(truncate_to_secs(T), datetime!(2022-10-27 22:06:34 UTC));
    }
}
