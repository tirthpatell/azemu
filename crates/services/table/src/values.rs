//! Property values (the Edm types Table Storage knows) and how they are read
//! from and written to the JSON wire form.

use azemu_core::time::{self, OffsetDateTime};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::errors::{invalid_input, invalid_value, Res};
use crate::json::J;

/// Ticks are 100 ns units since 1970-01-01T00:00:00Z.
pub type Ticks = i64;

const TICKS_PER_SEC: i64 = 10_000_000;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Str(String),
    Int32(i32),
    Int64(i64),
    Double(f64),
    Bool(bool),
    DateTime(Ticks),
    /// Lower-case canonical text.
    Guid(String),
    Binary(Vec<u8>),
}

impl Value {
    /// Approximate stored size in bytes (strings count UTF-16 units twice).
    pub fn size(&self) -> usize {
        match self {
            Value::Str(s) => s.encode_utf16().count() * 2,
            Value::Binary(b) => b.len(),
            Value::Guid(_) => 16,
            Value::Int32(_) | Value::Bool(_) => 4,
            _ => 8,
        }
    }
}

pub fn ticks_from_time(t: OffsetDateTime) -> Ticks {
    (t.unix_timestamp_nanos() / 100) as Ticks
}

/// `2011-11-04T00:05:23.283Z`: up to 7 fraction digits, trailing zeros dropped,
/// no fraction at all when it is zero.
pub fn format_ticks(t: Ticks) -> String {
    let secs = t.div_euclid(TICKS_PER_SEC);
    let frac = t.rem_euclid(TICKS_PER_SEC);
    let dt = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let mut s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", dt.year(), dt.month() as u8, dt.day(), dt.hour(), dt.minute(), dt.second());
    if frac != 0 {
        let f = format!("{frac:07}");
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    s.push('Z');
    s
}

pub fn parse_datetime(s: &str) -> Option<Ticks> {
    let t = time::parse_iso8601(s.trim())?;
    Some(ticks_from_time(t))
}

pub fn parse_guid(s: &str) -> Option<String> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() != 36 {
        return None;
    }
    for (i, c) in b.iter().enumerate() {
        let dash = matches!(i, 8 | 13 | 18 | 23);
        if dash != (*c == b'-') || (!dash && !c.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(s.to_ascii_lowercase())
}

pub fn encode_binary(b: &[u8]) -> String {
    B64.encode(b)
}

pub fn decode_binary(s: &str) -> Option<Vec<u8>> {
    B64.decode(s.trim()).ok()
}

/// Property names follow identifier rules: a letter or `_`, then letters,
/// digits and `_` (Unicode letters are allowed).
pub fn valid_property_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Decode one property of an entity payload. `ann` is the `@odata.type`
/// annotation sent next to it. `Ok(None)` means null: the property is dropped.
pub fn decode_value(v: &J, ann: Option<&str>) -> Res<Option<Value>> {
    if matches!(v, J::Null) {
        return Ok(None);
    }
    let ty = match ann {
        None => None,
        Some(a) => Some(match a {
            "Edm.String" | "Edm.Int32" | "Edm.Int64" | "Edm.Double" | "Edm.Boolean" | "Edm.DateTime" | "Edm.Guid" | "Edm.Binary" => a,
            _ => return Err(invalid_value()),
        }),
    };
    let val = match (ty, v) {
        (None, J::Str(s)) | (Some("Edm.String"), J::Str(s)) => Value::Str(s.clone()),
        (None, J::Bool(b)) | (Some("Edm.Boolean"), J::Bool(b)) => Value::Bool(*b),
        (None, J::Num(n)) => {
            if n.contains(['.', 'e', 'E']) {
                Value::Double(n.parse::<f64>().map_err(|_| invalid_value())?)
            } else {
                Value::Int32(n.parse::<i32>().map_err(|_| invalid_value())?)
            }
        }
        (Some("Edm.Boolean"), J::Str(s)) => match s.to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(invalid_value()),
        },
        (Some("Edm.Int32"), J::Num(n)) | (Some("Edm.Int32"), J::Str(n)) => {
            Value::Int32(n.trim().parse::<i32>().map_err(|_| invalid_value())?)
        }
        (Some("Edm.Int64"), J::Num(n)) | (Some("Edm.Int64"), J::Str(n)) => {
            Value::Int64(n.trim().parse::<i64>().map_err(|_| invalid_value())?)
        }
        (Some("Edm.Double"), J::Num(n)) => Value::Double(n.parse::<f64>().map_err(|_| invalid_value())?),
        (Some("Edm.Double"), J::Str(s)) => Value::Double(parse_double_text(s).ok_or_else(invalid_value)?),
        (Some("Edm.DateTime"), J::Str(s)) => Value::DateTime(parse_datetime(s).ok_or_else(invalid_value)?),
        (Some("Edm.Guid"), J::Str(s)) => Value::Guid(parse_guid(s).ok_or_else(invalid_value)?),
        (Some("Edm.Binary"), J::Str(s)) => Value::Binary(decode_binary(s).ok_or_else(invalid_value)?),
        (_, J::Obj(_)) | (_, J::Arr(_)) => return Err(invalid_input()),
        _ => return Err(invalid_value()),
    };
    Ok(Some(val))
}

fn parse_double_text(s: &str) -> Option<f64> {
    match s.trim() {
        "NaN" => Some(f64::NAN),
        "Infinity" | "INF" => Some(f64::INFINITY),
        "-Infinity" | "-INF" => Some(f64::NEG_INFINITY),
        t => t.parse::<f64>().ok().filter(|f| f.is_finite()),
    }
}

/// How a value is written in a response: the JSON text and, when the payload
/// level needs one, the `@odata.type` annotation.
pub fn wire(v: &Value) -> (String, Option<&'static str>) {
    match v {
        Value::Str(s) => {
            let mut o = String::new();
            crate::json::write_str(&mut o, s);
            (o, None)
        }
        Value::Int32(i) => (i.to_string(), None),
        Value::Bool(b) => (b.to_string(), None),
        Value::Int64(i) => (format!("\"{i}\""), Some("Edm.Int64")),
        Value::Double(f) => {
            if f.is_nan() {
                ("\"NaN\"".into(), Some("Edm.Double"))
            } else if f.is_infinite() {
                (if *f > 0.0 { "\"Infinity\"" } else { "\"-Infinity\"" }.into(), Some("Edm.Double"))
            } else {
                (format!("{f:?}"), None)
            }
        }
        Value::DateTime(t) => (format!("\"{}\"", format_ticks(*t)), Some("Edm.DateTime")),
        Value::Guid(g) => (format!("\"{g}\""), Some("Edm.Guid")),
        Value::Binary(b) => (format!("\"{}\"", encode_binary(b)), Some("Edm.Binary")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_format_trims_zeros() {
        let t = parse_datetime("2011-11-04T00:05:23.283000Z").unwrap();
        assert_eq!(format_ticks(t), "2011-11-04T00:05:23.283Z");
        let t = parse_datetime("2011-11-04T00:05:23.000000Z").unwrap();
        assert_eq!(format_ticks(t), "2011-11-04T00:05:23Z");
        let t = parse_datetime("2020-01-02T03:04:05.1234567Z").unwrap();
        assert_eq!(format_ticks(t), "2020-01-02T03:04:05.1234567Z");
        let t = parse_datetime("1601-01-01T00:00:00Z").unwrap();
        assert_eq!(format_ticks(t), "1601-01-01T00:00:00Z");
        let t = parse_datetime("9999-12-31T23:59:59Z").unwrap();
        assert_eq!(format_ticks(t), "9999-12-31T23:59:59Z");
    }

    #[test]
    fn datetime_offsets_normalise_to_utc() {
        let t = parse_datetime("2021-06-08T12:00:00+05:30").unwrap();
        assert_eq!(format_ticks(t), "2021-06-08T06:30:00Z");
    }

    #[test]
    fn property_names() {
        assert!(valid_property_name("abc_1"));
        assert!(valid_property_name("_x"));
        assert!(valid_property_name("啊齄丂狛狜"));
        assert!(!valid_property_name("123"));
        assert!(!valid_property_name(""));
        assert!(!valid_property_name("a-b"));
        assert!(!valid_property_name("a b"));
    }

    #[test]
    fn decode_inferred_and_annotated() {
        let n = |s: &str| J::Num(s.into());
        assert_eq!(decode_value(&n("5"), None).unwrap(), Some(Value::Int32(5)));
        assert_eq!(decode_value(&n("5.0"), None).unwrap(), Some(Value::Double(5.0)));
        assert_eq!(decode_value(&n("1e3"), None).unwrap(), Some(Value::Double(1000.0)));
        assert!(decode_value(&n("2147483648"), None).is_err());
        assert_eq!(decode_value(&J::Str("9".into()), Some("Edm.Int64")).unwrap(), Some(Value::Int64(9)));
        assert_eq!(decode_value(&J::Str("False".into()), Some("Edm.Boolean")).unwrap(), Some(Value::Bool(false)));
        assert_eq!(decode_value(&J::Str("2.75".into()), Some("Edm.Double")).unwrap(), Some(Value::Double(2.75)));
        assert!(matches!(decode_value(&J::Str("NaN".into()), Some("Edm.Double")).unwrap(), Some(Value::Double(f)) if f.is_nan()));
        assert_eq!(decode_value(&J::Null, Some("Edm.String")).unwrap(), None);
        assert!(decode_value(&J::Str("9223372036854775808".into()), Some("Edm.Int64")).is_err());
        assert!(decode_value(&J::Str("x".into()), Some("Edm.Nope")).is_err());
        assert!(decode_value(&J::Arr(vec![]), None).is_err());
        assert_eq!(decode_value(&J::Str("YmluYXJ5".into()), Some("Edm.Binary")).unwrap(), Some(Value::Binary(b"binary".to_vec())));
    }

    #[test]
    fn wire_forms() {
        assert_eq!(wire(&Value::Double(5.0)), ("5.0".to_string(), None));
        assert_eq!(wire(&Value::Int64(7)), ("\"7\"".to_string(), Some("Edm.Int64")));
        assert_eq!(wire(&Value::Double(f64::NAN)).1, Some("Edm.Double"));
        assert_eq!(wire(&Value::Str("é".into())).0, "\"\\u00e9\"");
    }

    #[test]
    fn guid_parsing() {
        assert_eq!(parse_guid("C9DA6455-213D-42C9-9A79-3E9149A57833").as_deref(), Some("c9da6455-213d-42c9-9a79-3e9149a57833"));
        assert!(parse_guid("not-a-guid").is_none());
    }
}
