//! ISO 8601 durations as the Service Bus API writes them (`PT1M`, `PT10M1S`,
//! `P10675199DT2H48M5.4775807S`).

/// .NET `TimeSpan.MaxValue` in milliseconds (10675199 days 2:48:05.4775807). Anything at or
/// above this means "never".
pub const TIMESPAN_MAX_MS: i64 = 922_337_203_685_477;

/// Parse `PnDTnHnMnS` (also `PnW`, fractions on the seconds). Returns milliseconds;
/// `None` when the text is not a duration. A value at or above `TIMESPAN_MAX_MS` is returned
/// as `TIMESPAN_MAX_MS`.
pub fn parse(s: &str) -> Option<i64> {
    let s = s.trim();
    let rest = s.strip_prefix('P').or_else(|| s.strip_prefix('p'))?;
    if rest.is_empty() {
        return None;
    }
    let mut total_ms: f64 = 0.0;
    let mut in_time = false;
    let mut num = String::new();
    let mut seen_any = false;
    for c in rest.chars() {
        match c {
            'T' | 't' => {
                if !num.is_empty() {
                    return None;
                }
                in_time = true;
            }
            '0'..='9' | '.' | ',' => num.push(if c == ',' { '.' } else { c }),
            unit => {
                let n: f64 = num.parse().ok()?;
                num.clear();
                seen_any = true;
                let mult = match (in_time, unit.to_ascii_uppercase()) {
                    (false, 'W') => 7.0 * 86_400_000.0,
                    (false, 'D') => 86_400_000.0,
                    (true, 'H') => 3_600_000.0,
                    (true, 'M') => 60_000.0,
                    (true, 'S') => 1000.0,
                    _ => return None,
                };
                total_ms += n * mult;
            }
        }
    }
    if !num.is_empty() || !seen_any {
        return None;
    }
    if total_ms >= TIMESPAN_MAX_MS as f64 {
        return Some(TIMESPAN_MAX_MS);
    }
    Some(total_ms.round() as i64)
}

/// Format milliseconds the way the service does. `None` or the maximum prints the TimeSpan maximum.
pub fn format(ms: Option<i64>) -> String {
    let ms = match ms {
        None => return "P10675199DT2H48M5.4775807S".to_string(),
        Some(m) if m >= TIMESPAN_MAX_MS => return "P10675199DT2H48M5.4775807S".to_string(),
        Some(m) => m.max(0),
    };
    let days = ms / 86_400_000;
    let rem = ms % 86_400_000;
    let h = rem / 3_600_000;
    let m = (rem % 3_600_000) / 60_000;
    let s_ms = rem % 60_000;
    let mut out = String::from("P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if h > 0 || m > 0 || s_ms > 0 || days == 0 {
        out.push('T');
        if h > 0 {
            out.push_str(&format!("{h}H"));
        }
        if m > 0 {
            out.push_str(&format!("{m}M"));
        }
        if s_ms > 0 || (h == 0 && m == 0) {
            let secs = s_ms / 1000;
            let frac = s_ms % 1000;
            if frac == 0 {
                out.push_str(&format!("{secs}S"));
            } else {
                let f = format!("{frac:03}");
                out.push_str(&format!("{secs}.{}S", f.trim_end_matches('0')));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_the_service_sends() {
        assert_eq!(parse("PT1M"), Some(60_000));
        assert_eq!(parse("PT13S"), Some(13_000));
        assert_eq!(parse("PT11M2S"), Some(662_000));
        assert_eq!(parse("PT10M1S"), Some(601_000));
        assert_eq!(parse("P1D"), Some(86_400_000));
        assert_eq!(parse("P1DT2H"), Some(93_600_000));
        assert_eq!(parse("PT0.5S"), Some(500));
        assert_eq!(parse("P10675199DT2H48M5.4775807S"), Some(TIMESPAN_MAX_MS));
        assert_eq!(parse("P14D"), Some(14 * 86_400_000));
        assert_eq!(parse("PT"), None);
        assert_eq!(parse("1M"), None);
        assert_eq!(parse("PT1X"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn formats_like_the_service() {
        assert_eq!(format(Some(60_000)), "PT1M");
        assert_eq!(format(Some(13_000)), "PT13S");
        assert_eq!(format(Some(662_000)), "PT11M2S");
        assert_eq!(format(Some(601_000)), "PT10M1S");
        assert_eq!(format(None), "P10675199DT2H48M5.4775807S");
        assert_eq!(format(Some(86_400_000)), "P1D");
        assert_eq!(format(Some(93_600_000)), "P1DT2H");
        assert_eq!(format(Some(500)), "PT0.5S");
        assert_eq!(format(Some(0)), "PT0S");
        assert_eq!(format(Some(600_000)), "PT10M");
        assert_eq!(format(Some(3_600_000)), "PT1H");
    }
}
