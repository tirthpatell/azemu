//! Time formats of the App Configuration wire protocol.

use azemu_core::time::{self, OffsetDateTime};

/// `2026-01-14T17:45:13+00:00` (what `last_modified` looks like).
pub fn iso_secs(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

/// `2026-09-01T17:43:02.3424828+00:00` (a snapshot's `created` while it is provisioning,
/// and `expires`).
pub fn iso_7(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}+00:00",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond() / 100
    )
}

/// Drop the sub-second part.
pub fn trunc(t: OffsetDateTime) -> OffsetDateTime {
    time::truncate_to_secs(t)
}

/// `Accept-Datetime` as clients send it: RFC 1123 (`Sat, 01 Apr 2000 09:30:45 GMT`) or
/// the Python `str(datetime)` form (`2000-04-01 09:30:45+00:00`), or ISO 8601.
pub fn parse_datetime(s: &str) -> Option<OffsetDateTime> {
    time::parse_rfc1123(s).or_else(|| time::parse_iso8601(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let t = time::parse_iso8601("2026-09-01T17:43:02.3424828Z").unwrap();
        assert_eq!(iso_secs(t), "2026-09-01T17:43:02+00:00");
        assert_eq!(iso_7(t), "2026-09-01T17:43:02.3424828+00:00");
    }

    #[test]
    fn parses_the_forms_clients_send() {
        let a = parse_datetime("Sat, 01 Apr 2000 09:30:45 GMT").unwrap();
        let b = parse_datetime("2000-04-01 09:30:45+00:00").unwrap();
        let c = parse_datetime("2000-04-01T09:30:45.123456+00:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(trunc(c), a);
        assert!(parse_datetime("yesterday").is_none());
    }
}
