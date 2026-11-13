//! Small formatting helpers: timestamps, ETags, TimeSpan text, XML escaping.

use azemu_core::time::OffsetDateTime;

/// Timestamp the service prints for "never".
pub const ZERO_TS: &str = "0001-01-01T00:00:00Z";

fn dt(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

pub fn ms_of(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

/// `2024-09-04T18:17:36` (no zone, no fraction).
pub fn plain(ms: i64) -> String {
    let t = dt(ms);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

/// Atom `published` / `updated`: whole seconds.
pub fn ts_secs(ms: i64) -> String {
    format!("{}Z", plain(ms))
}

/// Queue / topic `CreatedAt`: milliseconds with trailing zeros trimmed (`...36.28Z`, `...36Z`).
pub fn ts_trimmed(ms: i64) -> String {
    let frac = ms.rem_euclid(1000);
    if frac == 0 {
        return ts_secs(ms);
    }
    let f = format!("{frac:03}");
    format!("{}.{}Z", plain(ms), f.trim_end_matches('0'))
}

/// Subscription / rule timestamps: seven fraction digits.
pub fn ts_7(ms: i64) -> String {
    format!("{}.{:03}0000Z", plain(ms), ms.rem_euclid(1000))
}

/// The ETag Service Bus sends: .NET ticks (100 ns since 0001-01-01) of the entity's update time.
pub fn etag_of(ms: i64) -> String {
    (i128::from(ms) * 10_000 + 621_355_968_000_000_000).to_string()
}

/// .NET `TimeSpan.ToString()`: `25.00:00:00`, `00:05:00`.
pub fn timespan_text(ms: i64) -> String {
    let ms = ms.max(0);
    let days = ms / 86_400_000;
    let rem = ms % 86_400_000;
    let (h, m, s, f) = (rem / 3_600_000, (rem % 3_600_000) / 60_000, (rem % 60_000) / 1000, rem % 1000);
    let mut out = String::new();
    if days > 0 {
        out.push_str(&format!("{days}."));
    }
    out.push_str(&format!("{h:02}:{m:02}:{s:02}"));
    if f > 0 {
        out.push_str(&format!(".{f:03}0000"));
    }
    out
}

/// Escape text content (and attribute values): `& < > "`, and CR as a character reference
/// (the service writes `&#xD;` in its error texts).
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    out
}

/// A name as it stands in an `id` or a link. A `/` inside a name stays a `/` (Azure answers a queue made as `txt%2F.-_123` with the id
/// `https://.../txt/.-_123?api-version=2021-05`; recording `async/test_mgmt_queues_async.pyTestServiceBusAdministrationClientQueueAsynctest_async_mgmt_queue_delete_basic.json`,
/// entries 3 and 4).
pub fn enc_segment(name: &str) -> String {
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~').remove(b'$').remove(b'/');
    percent_encoding::utf8_percent_encode(name, SET).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slash_in_a_name_stays_a_slash_in_an_id() {
        assert_eq!(enc_segment("txt/.-_123"), "txt/.-_123");
        assert_eq!(enc_segment("a b"), "a%20b");
    }

    #[test]
    fn timestamps() {
        // 2024-09-04T18:17:36.280Z
        let ms = 1_725_473_856_280;
        assert_eq!(ts_secs(ms), "2024-09-04T18:17:36Z");
        assert_eq!(ts_trimmed(ms), "2024-09-04T18:17:36.28Z");
        assert_eq!(ts_trimmed(ms - 280), "2024-09-04T18:17:36Z");
        assert_eq!(ts_7(ms), "2024-09-04T18:17:36.2800000Z");
        assert_eq!(etag_of(ms), "638610706562800000");
    }

    #[test]
    fn timespans() {
        assert_eq!(timespan_text(25 * 86_400_000), "25.00:00:00");
        assert_eq!(timespan_text(300_000), "00:05:00");
        assert_eq!(timespan_text(20_500), "00:00:20.5000000");
    }
}
