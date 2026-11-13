//! A small JSON reader and writer that keep the order of object keys, which
//! Table Storage echoes back (property order is the order the client sent).

/// A parsed JSON value. Numbers keep their source text so that `5` (an
/// Edm.Int32) and `5.0` (an Edm.Double) stay different.
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
}

const MAX_DEPTH: usize = 64;

/// Parse a complete JSON document. A leading BOM is skipped.
pub fn parse(text: &str) -> Result<J, ()> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser { s: text.as_bytes(), pos: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.pos != p.s.len() {
        return Err(());
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && matches!(self.s[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn lit(&mut self, word: &str, v: J) -> Result<J, ()> {
        if self.s[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(())
        }
    }

    fn value(&mut self, depth: usize) -> Result<J, ()> {
        if depth > MAX_DEPTH {
            return Err(());
        }
        match self.peek().ok_or(())? {
            b'{' => {
                self.pos += 1;
                let mut out: Vec<(String, J)> = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return Ok(J::Obj(out));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err(());
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return Err(());
                    }
                    self.pos += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    // A repeated key replaces the earlier value in place.
                    if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
                        slot.1 = v;
                    } else {
                        out.push((k, v));
                    }
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(J::Obj(out));
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                let mut out = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Ok(J::Arr(out));
                }
                loop {
                    self.ws();
                    out.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(J::Arr(out));
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'"' => Ok(J::Str(self.string()?)),
            b't' => self.lit("true", J::Bool(true)),
            b'f' => self.lit("false", J::Bool(false)),
            b'n' => self.lit("null", J::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(()),
        }
    }

    fn number(&mut self) -> Result<J, ()> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let digits = |p: &mut Self| {
            let b = p.pos;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.pos += 1;
            }
            p.pos > b
        };
        if !digits(self) {
            return Err(());
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !digits(self) {
                return Err(());
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(());
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.pos]).map_err(|_| ())?;
        Ok(J::Num(text.to_string()))
    }

    fn hex4(&mut self) -> Result<u32, ()> {
        if self.pos + 4 > self.s.len() {
            return Err(());
        }
        let t = std::str::from_utf8(&self.s[self.pos..self.pos + 4]).map_err(|_| ())?;
        let v = u32::from_str_radix(t, 16).map_err(|_| ())?;
        self.pos += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ()> {
        self.pos += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = self.peek().ok_or(())?;
            self.pos += 1;
            match b {
                b'"' => return String::from_utf8(out).map_err(|_| ()),
                b'\\' => {
                    let e = self.peek().ok_or(())?;
                    self.pos += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                if self.s[self.pos..].starts_with(b"\\u") {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(());
                                    }
                                    char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)).ok_or(())?
                                } else {
                                    return Err(());
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(());
                            } else {
                                char::from_u32(hi).ok_or(())?
                            }
                        }
                        _ => return Err(()),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                0..=0x1f => return Err(()),
                _ => out.push(b),
            }
        }
    }
}

/// Append `s` as a JSON string literal. Non-ASCII characters are written as
/// `\uXXXX` escapes, the way the service does.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Incremental writer for one JSON object, in insertion order.
pub struct ObjWriter {
    buf: String,
    first: bool,
}

impl Default for ObjWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjWriter {
    pub fn new() -> ObjWriter {
        ObjWriter { buf: String::from("{"), first: true }
    }

    fn key(&mut self, k: &str) {
        if !self.first {
            self.buf.push(',');
        }
        self.first = false;
        write_str(&mut self.buf, k);
        self.buf.push(':');
    }

    pub fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        write_str(&mut self.buf, v);
    }

    /// `raw` must already be valid JSON.
    pub fn raw(&mut self, k: &str, raw: &str) {
        self.key(k);
        self.buf.push_str(raw);
    }

    pub fn finish(mut self) -> String {
        self.buf.push('}');
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_and_number_text() {
        let j = parse(r#"{"b": 1, "a": 2.0, "c": [true, null, "x"], "b": 3}"#).unwrap();
        assert_eq!(
            j,
            J::Obj(vec![
                ("b".into(), J::Num("3".into())),
                ("a".into(), J::Num("2.0".into())),
                ("c".into(), J::Arr(vec![J::Bool(true), J::Null, J::Str("x".into())])),
            ])
        );
    }

    #[test]
    fn string_escapes_and_surrogates() {
        let j = parse(r#""a\u00e9\ud83d\ude42\n\"""#).unwrap();
        assert_eq!(j, J::Str("a\u{e9}\u{1f642}\n\"".into()));
        let mut s = String::new();
        write_str(&mut s, "a\u{e9}\u{1f642}\n\"");
        assert_eq!(s, "\"a\\u00e9\\ud83d\\ude42\\n\\\"\"");
    }

    #[test]
    fn rejects_bad_documents() {
        for bad in ["", "{", "{\"a\"}", "[1,]", "{\"a\":1,}", "tru", "\"x", "1 2", "{\"a\":01x}", "\"\\ud800\""] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bom_is_skipped() {
        assert!(parse("\u{feff}{}").is_ok());
    }
}
