//! Minimal ISO 8601 / RFC 3339 timestamp parsing (for `SET` conversions).

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction][Z|+HH:MM|-HH:MM]` (no offset = UTC)
/// into milliseconds since the Unix epoch.
pub(crate) fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if !s.is_ascii() || s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(&s[0..4])?, num(&s[5..7])?, num(&s[8..10])?);
    let (h, mi, sec) = (num(&s[11..13])?, num(&s[14..16])?, num(&s[17..19])?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let first3: String = digits.chars().chain("000".chars()).take(3).collect();
        millis = num(&first3)?;
        rest = &frac[digits.len()..];
    }
    let offset_min = if rest.is_empty() || rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let body = &rest[1..];
        let (oh, om) = match body.len() {
            5 if body.as_bytes()[2] == b':' => (num(&body[0..2])?, num(&body[3..5])?),
            4 => (num(&body[0..2])?, num(&body[2..4])?),
            2 => (num(body)?, 0),
            _ => return None,
        };
        sign * (oh * 60 + om)
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    secs.checked_mul(1000)?.checked_add(millis)
}
