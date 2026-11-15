//! The receiver's selector filter (`apache.org:selector-filter:string`).
//!
//! The SDKs send one comparison: `amqp.annotation.x-opt-offset > '123'`,
//! `amqp.annotation.x-opt-sequence-number >= 5`, `amqp.annotation.x-opt-enqueued-time > '1700000000000'`.
//! The value is quoted or not; `-1` is the start of the stream and `@latest` means "only new events".

use crate::errors::EhError;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StartPosition {
    /// No filter, or offset `-1`.
    Beginning,
    /// `@latest`
    Latest,
    Offset {
        value: i64,
        inclusive: bool,
    },
    Sequence {
        value: i64,
        inclusive: bool,
    },
    /// Milliseconds since the epoch.
    Time {
        ms: i64,
        inclusive: bool,
    },
}

pub fn parse(expr: &str) -> Result<StartPosition, EhError> {
    // Azure refuses every filter it cannot read with `amqp:not-allowed` and this text (checked in November 2026, with an offset
    // that is not a number)
    let bad = || EhError::not_allowed(format!("The filter '{expr}' is not a valid amqp filter for EventHub entities"));
    let e = expr.trim();
    let op_at = e.find(['>', '<', '=']).ok_or_else(bad)?;
    let name = e[..op_at].trim();
    let rest = &e[op_at..];
    let (inclusive, value) = if let Some(v) = rest.strip_prefix(">=") {
        (true, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (false, v)
    } else {
        return Err(bad());
    };
    let value = value.trim().trim_matches(|c| c == '\'' || c == '"').trim();
    let name = name.strip_prefix("amqp.annotation.").unwrap_or(name).to_ascii_lowercase();
    let number = || value.parse::<i64>().map_err(|_| bad());
    match name.as_str() {
        "x-opt-offset" => {
            if value.eq_ignore_ascii_case("@latest") {
                Ok(StartPosition::Latest)
            } else {
                let v = number()?;
                if v < 0 {
                    Ok(StartPosition::Beginning)
                } else {
                    Ok(StartPosition::Offset { value: v, inclusive })
                }
            }
        }
        "x-opt-sequence-number" => Ok(StartPosition::Sequence { value: number()?, inclusive }),
        "x-opt-enqueued-time" => Ok(StartPosition::Time { ms: number()?, inclusive }),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_expressions() {
        assert_eq!(parse("amqp.annotation.x-opt-offset > '-1'").unwrap(), StartPosition::Beginning);
        assert_eq!(parse("amqp.annotation.x-opt-offset > '@latest'").unwrap(), StartPosition::Latest);
        assert_eq!(parse("amqp.annotation.x-opt-offset >= '123'").unwrap(), StartPosition::Offset { value: 123, inclusive: true });
        assert_eq!(parse("amqp.annotation.x-opt-sequence-number > 5").unwrap(), StartPosition::Sequence { value: 5, inclusive: false });
        assert_eq!(
            parse("amqp.annotation.x-opt-enqueued-time>'1700000000000'").unwrap(),
            StartPosition::Time { ms: 1_700_000_000_000, inclusive: false }
        );
        assert!(parse("amqp.annotation.x-opt-offset > 'abc'").is_err());
        assert!(parse("amqp.annotation.x-opt-foo > '1'").is_err());
        assert!(parse("nonsense").is_err());
    }
}
