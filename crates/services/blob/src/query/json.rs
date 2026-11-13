//! JSON for the query engine: a strict parser that keeps the order of object fields (the service
//! returns `SELECT *` records as written), and a compact writer.
//!
//! Error messages follow the one recorded from the service for `{name: owner}`:
//! `Unexpected token 'n' at [byte: 1]. Expecting token '"'. ` (the byte is counted from the start of the record).

use super::value::Value;

/// Documents nested deeper than this are refused (the parser calls itself once per level).
pub const MAX_DEPTH: usize = 100;

pub fn parse(b: &[u8]) -> Result<Value, String> {
    let mut p = P { b, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i < b.len() {
        return Err(p.unexpected(None));
    }
    Ok(v)
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn unexpected(&self, expecting: Option<&str>) -> String {
        match self.b.get(self.i) {
            None => format!("Unexpected end of input at [byte: {}]. ", self.i),
            Some(_) => {
                let ch = String::from_utf8_lossy(&self.b[self.i..(self.i + 4).min(self.b.len())]).chars().next().unwrap_or('?');
                match expecting {
                    Some(e) => format!("Unexpected token '{ch}' at [byte: {}]. Expecting token {e}. ", self.i),
                    None => format!("Unexpected token '{ch}' at [byte: {}]. ", self.i),
                }
            }
        }
    }

    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            // point at the first byte that differs
            let n = self.b[self.i..].iter().zip(word.bytes()).take_while(|(a, b)| *a == b).count();
            self.i += n;
            Err(self.unexpected(None))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("The document is nested more than {MAX_DEPTH} levels deep at [byte: {}]. ", self.i));
        }
        match self.b.get(self.i) {
            None => Err(self.unexpected(None)),
            Some(b'{') => {
                self.i += 1;
                let mut fields: Vec<(String, Value)> = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::Object(fields));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return Err(self.unexpected(Some("'\"'")));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return Err(self.unexpected(Some("':'")));
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    // a repeated name: the last one wins, in the place of the first
                    match fields.iter_mut().find(|(n, _)| *n == k) {
                        Some(slot) => slot.1 = v,
                        None => fields.push((k, v)),
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Object(fields));
                        }
                        _ => return Err(self.unexpected(Some("',' or '}'"))),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Array(items));
                        }
                        _ => return Err(self.unexpected(Some("',' or ']'"))),
                    }
                }
            }
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'n') => self.lit("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.unexpected(None)),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let st = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.i < p.b.len() && p.b[p.i].is_ascii_digit() {
                p.i += 1;
            }
            p.i > s
        };
        if !digits(self) {
            return Err(self.unexpected(None));
        }
        let mut float = false;
        if self.b.get(self.i) == Some(&b'.') {
            float = true;
            self.i += 1;
            if !digits(self) {
                return Err(self.unexpected(None));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            float = true;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.unexpected(None));
            }
        }
        let text = std::str::from_utf8(&self.b[st..self.i]).unwrap_or("0");
        if !float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            _ => Err(format!("The number at [byte: {st}] is out of range. ")),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut n = 0;
        for _ in 0..4 {
            let c = *self.b.get(self.i).ok_or_else(|| self.unexpected(None))?;
            let d = (c as char).to_digit(16).ok_or_else(|| self.unexpected(None))?;
            n = n * 16 + d;
            self.i += 1;
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.b.get(self.i) else { return Err(self.unexpected(None)) };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else { return Err(self.unexpected(None)) };
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.b[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = save;
                                }
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.unexpected(None));
                        }
                    }
                }
                0..=0x1f => return Err(self.unexpected(None)),
                _ => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }
}

/// Compact JSON. NaN and infinities are `null`, timestamps are strings, MISSING fields are left out.
pub fn write(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Missing | Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Float(f) => {
            if f.is_finite() {
                let s = format!("{f}");
                out.extend_from_slice(s.as_bytes());
            } else {
                out.extend_from_slice(b"null");
            }
        }
        Value::Str(s) => write_str(s, out),
        Value::Ts(t) => write_str(&super::datetime::format(t), out),
        Value::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(x, out);
            }
            out.push(b']');
        }
        Value::Object(o) => {
            out.push(b'{');
            let mut first = true;
            for (k, x) in o {
                if matches!(x, Value::Missing) {
                    continue;
                }
                if !first {
                    out.push(b',');
                }
                first = false;
                write_str(k, out);
                out.push(b':');
                write(x, out);
            }
            out.push(b'}');
        }
    }
}

pub fn write_str(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes()),
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        let v = parse(s.as_bytes()).unwrap();
        let mut o = vec![];
        write(&v, &mut o);
        String::from_utf8(o).unwrap()
    }

    #[test]
    fn keeps_field_order_and_numbers() {
        assert_eq!(
            rt(r#" {"name": "owner", "id": 1, "z": [1, 2.5, -3e2, true, null], "a": {}} "#),
            r#"{"name":"owner","id":1,"z":[1,2.5,-300,true,null],"a":{}}"#
        );
        assert_eq!(rt(r#""aé😀\n""#), "\"aé😀\\n\"");
        assert_eq!(rt(r#"{"a":1,"a":2}"#), r#"{"a":2}"#);
    }

    #[test]
    fn the_recorded_error_message() {
        // from the recording of the Python SDK's fatal error test: `{name: owner}` is not JSON
        assert_eq!(parse(b"{name: owner}").unwrap_err(), "Unexpected token 'n' at [byte: 1]. Expecting token '\"'. ");
        assert!(parse(b"{\"a\" 1}").unwrap_err().contains("Expecting token ':'"));
        assert!(parse(b"{\"a\":1").unwrap_err().starts_with("Unexpected end of input"));
        assert!(parse(b"[1 2]").is_err());
        assert!(parse(b"tru").is_err());
        assert!(parse(b"1 2").is_err());
        assert!(parse(b"\"a\nb\"").is_err(), "a raw line break is not allowed in a string");
    }

    #[test]
    fn deep_documents_are_refused_not_a_crash() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse(deep.as_bytes()).unwrap_err().contains("nested"));
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse(ok.as_bytes()).is_ok());
    }
}
