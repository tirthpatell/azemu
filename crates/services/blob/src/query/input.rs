//! Reading the blob record by record: a window over the blob's stored pieces (the blob is never copied
//! whole), a delimited text reader and a JSON line reader.

use crate::state::Piece;

/// How much of the blob is copied into the window at a time.
const CHUNK: usize = 64 * 1024;

/// A fatal problem with the input (the query ends, the answer carries it as an error record).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadError {
    pub name: &'static str,
    pub message: String,
    /// Blob offset of the record.
    pub position: u64,
}

/// A forward-only reader over the blob's pieces. Only the unread part of the current window is kept.
pub struct Stream {
    pieces: Vec<Piece>,
    pi: usize,
    po: usize,
    buf: Vec<u8>,
    start: usize,
    /// Blob offset of `buf[start]`.
    base: u64,
    total: u64,
}

impl Stream {
    pub fn new(pieces: Vec<Piece>) -> Stream {
        let total = pieces.iter().map(|p| p.len() as u64).sum();
        Stream { pieces, pi: 0, po: 0, buf: Vec::new(), start: 0, base: 0, total }
    }

    #[cfg(test)]
    pub fn from_bytes(b: &[u8]) -> Stream {
        Stream::new(vec![Piece::Data(azemu_core::bytes::Bytes::copy_from_slice(b))])
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Offset of the next unread byte.
    pub fn offset(&self) -> u64 {
        self.base
    }

    fn pull(&mut self) -> bool {
        while self.pi < self.pieces.len() {
            let len = self.pieces[self.pi].len();
            if self.po >= len {
                self.pi += 1;
                self.po = 0;
                continue;
            }
            let n = (len - self.po).min(CHUNK);
            match &self.pieces[self.pi] {
                Piece::Data(d) => self.buf.extend_from_slice(&d[self.po..self.po + n]),
                Piece::Zeros(_) => self.buf.resize(self.buf.len() + n, 0),
            }
            self.po += n;
            return true;
        }
        false
    }

    /// At least `want` unread bytes are in the window, unless the blob ends first.
    pub fn fill(&mut self, want: usize) {
        while self.buf.len() - self.start < want {
            if self.start > 0 && self.start >= self.buf.len() / 2 {
                self.buf.drain(..self.start);
                self.start = 0;
            }
            if !self.pull() {
                break;
            }
        }
    }

    /// The unread bytes in the window (call `fill` first).
    pub fn avail(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    /// Bytes held in the window (tests check that it stays small however big the blob is).
    #[cfg(test)]
    pub fn window_len(&self) -> usize {
        self.buf.len()
    }

    pub fn consume(&mut self, n: usize) {
        let n = n.min(self.buf.len() - self.start);
        self.start += n;
        self.base += n as u64;
    }

    pub fn at_end(&mut self) -> bool {
        self.fill(1);
        self.avail().is_empty()
    }

    fn at(&mut self, pat: &[u8]) -> bool {
        !pat.is_empty() && {
            self.fill(pat.len());
            self.avail().starts_with(pat)
        }
    }
}

/// Settings of delimited text, input or output.
#[derive(Debug, Clone, PartialEq)]
pub struct Delimited {
    pub column: Vec<u8>,
    pub quote: Vec<u8>,
    pub record: Vec<u8>,
    /// Empty: no escape character.
    pub escape: Vec<u8>,
    pub headers: bool,
}

impl Default for Delimited {
    fn default() -> Delimited {
        Delimited { column: b",".to_vec(), quote: b"\"".to_vec(), record: b"\n".to_vec(), escape: vec![], headers: false }
    }
}

fn first_of(pats: &[&[u8]]) -> Vec<u8> {
    let mut v: Vec<u8> = pats.iter().filter_map(|p| p.first().copied()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

impl Delimited {
    /// Length of the record terminator at the current position. With the default `\n`, `\r\n` ends a record too
    /// (the recorded blobs of the SDK tests have `\r\n` line ends and the service returns them as `\n` records).
    fn record_end(&self, s: &mut Stream) -> Option<usize> {
        if self.record == b"\n" {
            if s.at(b"\r\n") {
                return Some(2);
            }
            if s.at(b"\n") {
                return Some(1);
            }
            return None;
        }
        if s.at(&self.record) {
            Some(self.record.len())
        } else {
            None
        }
    }

    fn too_big(&self, start: u64, max: usize) -> ReadError {
        ReadError { name: "RecordTooLarge", message: format!("A record is larger than {max} bytes."), position: start }
    }

    /// The next record, with the blob offset it starts at. Empty lines are skipped.
    pub fn next(&self, s: &mut Stream, max: usize) -> Result<Option<(u64, Vec<String>)>, ReadError> {
        // an empty line is not a record
        loop {
            if s.at_end() {
                return Ok(None);
            }
            match self.record_end(s) {
                Some(n) => s.consume(n),
                None => break,
            }
        }
        let start = s.offset();
        let mut fields: Vec<String> = Vec::new();
        let stop: Vec<u8> = first_of(&[&self.column, &self.record, b"\r"]);
        let inside: Vec<u8> = first_of(&[&self.quote, &self.escape]);
        let text = |f: Vec<u8>| String::from_utf8(f).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        loop {
            let mut field: Vec<u8> = Vec::new();
            if s.at(&self.quote) {
                s.consume(self.quote.len());
                loop {
                    s.fill(self.escape.len() + self.quote.len() + 1);
                    if s.avail().is_empty() {
                        return Err(ReadError {
                            name: "ParseError",
                            message: "The record ends inside a quoted field.".into(),
                            position: start,
                        });
                    }
                    if s.at(&self.escape) {
                        let (e, q) = (self.escape.len(), self.quote.len());
                        if s.avail()[e..].starts_with(&self.quote) {
                            field.extend_from_slice(&self.quote);
                            s.consume(e + q);
                        } else if s.avail()[e..].starts_with(&self.escape) {
                            field.extend_from_slice(&self.escape);
                            s.consume(e + e);
                        } else {
                            field.extend_from_slice(&self.escape);
                            s.consume(e);
                        }
                    } else if s.at(&self.quote) {
                        s.consume(self.quote.len());
                        // a doubled quote is a quote
                        if self.escape.is_empty() && s.at(&self.quote) {
                            field.extend_from_slice(&self.quote);
                            s.consume(self.quote.len());
                        } else {
                            break;
                        }
                    } else {
                        // up to the next byte that could start a quote or an escape
                        let a = s.avail();
                        let n = a.iter().position(|b| inside.contains(b)).unwrap_or(a.len()).max(1);
                        field.extend_from_slice(&a[..n]);
                        s.consume(n);
                    }
                    if (s.offset() - start) as usize > max {
                        return Err(self.too_big(start, max));
                    }
                }
            }
            // the rest of the field, up to a column or record separator
            loop {
                s.fill(self.column.len().max(self.record.len()) + 2);
                if s.avail().is_empty() || s.at(&self.column) || self.record_end(s).is_some() {
                    break;
                }
                let a = s.avail();
                // the byte under the cursor is not a separator (checked above): take it, then the run after it
                let n = 1 + a[1..].iter().position(|b| stop.contains(b)).unwrap_or(a.len() - 1);
                field.extend_from_slice(&a[..n]);
                s.consume(n);
                if (s.offset() - start) as usize > max {
                    return Err(self.too_big(start, max));
                }
            }
            fields.push(text(field));
            if s.at(&self.column) {
                s.consume(self.column.len());
                continue;
            }
            if let Some(n) = self.record_end(s) {
                s.consume(n);
            }
            return Ok(Some((start, fields)));
        }
    }
}

/// The next JSON document, with the blob offset it starts at.
///
/// The documents of a blob are not split at the record separator but read one after the other: the separator
/// (and white space) between two documents is skipped, and is not needed. The real service reads
/// `{"a":1}{"b":2}` as two records although no separator is between them (recorded:
/// `test_quick_query_with_only_input_json_serialization_setting`), and a separator inside a string is data.
/// An object or array ends where its brackets close; anything else ends at white space or the separator.
pub fn next_json_record(s: &mut Stream, sep: &[u8], max: usize) -> Result<Option<(u64, Vec<u8>)>, ReadError> {
    // between documents
    loop {
        s.fill(sep.len().max(1));
        let Some(&c) = s.avail().first() else { return Ok(None) };
        if c.is_ascii_whitespace() {
            s.consume(1);
        } else if s.at(sep) {
            s.consume(sep.len());
        } else {
            break;
        }
    }
    let start = s.offset();
    let too_big = || ReadError { name: "RecordTooLarge", message: format!("A record is larger than {max} bytes."), position: start };
    let mut rec: Vec<u8> = Vec::new();
    let structured = matches!(s.avail().first(), Some(b'{' | b'['));
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    'scan: loop {
        s.fill(sep.len().max(1) + 1);
        if s.avail().is_empty() {
            break;
        }
        let mut n = 0;
        let a = s.avail();
        while n < a.len() {
            let c = a[n];
            if structured {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == b'\\' {
                        escaped = true;
                    } else if c == b'"' {
                        in_string = false;
                    }
                } else {
                    match c {
                        b'"' => in_string = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                n += 1;
                                rec.extend_from_slice(&a[..n]);
                                s.consume(n);
                                break 'scan;
                            }
                        }
                        _ => {}
                    }
                }
            } else if c.is_ascii_whitespace() || (!sep.is_empty() && a[n..].starts_with(sep)) {
                break;
            }
            n += 1;
        }
        if !structured && n < a.len() {
            rec.extend_from_slice(&a[..n]);
            s.consume(n);
            break;
        }
        rec.extend_from_slice(&a[..n]);
        s.consume(n);
        if rec.len() > max {
            return Err(too_big());
        }
    }
    if rec.len() > max {
        return Err(too_big());
    }
    Ok(Some((start, rec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv(d: &Delimited, data: &str) -> Vec<Vec<String>> {
        let mut s = Stream::from_bytes(data.as_bytes());
        let mut out = vec![];
        while let Some((_, r)) = d.next(&mut s, 1 << 20).unwrap() {
            out.push(r);
        }
        out
    }

    fn rows(r: &[&[&str]]) -> Vec<Vec<String>> {
        r.iter().map(|x| x.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn plain_and_crlf() {
        let d = Delimited::default();
        assert_eq!(csv(&d, "a,b\r\nc,d\n\n\ne"), rows(&[&["a", "b"], &["c", "d"], &["e"]]));
        assert_eq!(csv(&d, "a,,\n"), rows(&[&["a", "", ""]]));
        assert_eq!(csv(&d, ""), rows(&[]));
        assert_eq!(csv(&d, "lone\rcr,x"), rows(&[&["lone\rcr", "x"]]));
    }

    #[test]
    fn quotes_doubled_and_escaped() {
        let d = Delimited::default();
        assert_eq!(csv(&d, "\"a,b\",\"say \"\"hi\"\"\",\"line\nbreak\"\nz"), rows(&[&["a,b", "say \"hi\"", "line\nbreak"], &["z"]]));
        assert_eq!(csv(&d, "\"ab\"cd,e"), rows(&[&["abcd", "e"]]));
        assert_eq!(csv(&d, "ab\"cd,e"), rows(&[&["ab\"cd", "e"]]));
        let e = Delimited { escape: b"\\".to_vec(), quote: b"'".to_vec(), ..Delimited::default() };
        assert_eq!(csv(&e, "'it\\'s','a\\\\b','c\\d'"), rows(&[&["it's", "a\\b", "c\\d"]]));
        let mut s = Stream::from_bytes(b"\"open,x");
        assert_eq!(Delimited::default().next(&mut s, 100).unwrap_err().name, "ParseError");
    }

    #[test]
    fn other_separators_of_any_length() {
        let d = Delimited { column: b"||".to_vec(), record: b";;".to_vec(), quote: b"^".to_vec(), ..Delimited::default() };
        assert_eq!(csv(&d, "a||b;;c||^x;;y^||d;;"), rows(&[&["a", "b"], &["c", "x;;y", "d"]]));
        let d = Delimited { column: b"\t".to_vec(), record: b"\r\n".to_vec(), ..Delimited::default() };
        assert_eq!(csv(&d, "a\tb\r\nc\td"), rows(&[&["a", "b"], &["c", "d"]]));
    }

    #[test]
    fn offsets_and_limits_and_window_edges() {
        let d = Delimited::default();
        let mut s = Stream::from_bytes(b"ab,c\r\nxyz\n");
        let (o1, _) = d.next(&mut s, 100).unwrap().unwrap();
        let (o2, _) = d.next(&mut s, 100).unwrap().unwrap();
        assert_eq!((o1, o2, s.offset()), (0, 6, 10));
        let mut s = Stream::from_bytes(&vec![b'a'; 5000]);
        assert_eq!(d.next(&mut s, 1000).unwrap_err().name, "RecordTooLarge");
        // a blob in many pieces, separators and quotes cut by the piece and window edges
        let line = "\"q,1\",abc\r\n";
        let data = line.repeat(30_000);
        let pieces: Vec<Piece> = data.as_bytes().chunks(7).map(|c| Piece::Data(azemu_core::bytes::Bytes::copy_from_slice(c))).collect();
        let mut s = Stream::new(pieces);
        let mut n = 0;
        while let Some((off, r)) = d.next(&mut s, 100).unwrap() {
            assert_eq!(r, vec!["q,1".to_string(), "abc".to_string()]);
            assert_eq!(off, (n * line.len()) as u64);
            n += 1;
        }
        assert_eq!(n, 30_000);
    }

    #[test]
    fn the_window_stays_small_over_a_big_blob() {
        // 32 MiB in 1 MiB pieces; only the window is ever copied
        let piece = azemu_core::bytes::Bytes::from("0123456789,abcdefghij\n".repeat(47_000));
        let pieces: Vec<Piece> = (0..32).map(|_| Piece::Data(piece.clone())).collect();
        let mut s = Stream::new(pieces);
        let d = Delimited::default();
        let (mut n, mut widest) = (0u64, 0);
        while d.next(&mut s, 100).unwrap().is_some() {
            n += 1;
            widest = widest.max(s.window_len());
        }
        assert_eq!(n, 47_000 * 32);
        assert!(widest <= 2 * CHUNK + 64, "the window grew to {widest} bytes");
    }

    #[test]
    fn json_records() {
        let mut s = Stream::from_bytes(b"{\"a\":1}\n\n  \n{\"b\":2}\r\n{\"c\":3}");
        let mut v = vec![];
        while let Some((o, r)) = next_json_record(&mut s, b"\n", 100).unwrap() {
            v.push((o, String::from_utf8(r).unwrap()));
        }
        assert_eq!(v, vec![(0, "{\"a\":1}".to_string()), (12, "{\"b\":2}".to_string()), (21, "{\"c\":3}".to_string())]);
        // no separator needed between documents (recorded), a separator or line break inside a string is data,
        // and a document may span lines
        let mut s = Stream::from_bytes(b"{\"a\": 1}{\"b\": \"x;y\\\"}\"}\n[1,\n 2] {\"c\":[{}]}");
        let mut v = vec![];
        while let Some((_, r)) = next_json_record(&mut s, b";", 100).unwrap() {
            v.push(String::from_utf8(r).unwrap());
        }
        assert_eq!(v, vec!["{\"a\": 1}", "{\"b\": \"x;y\\\"}\"}", "[1,\n 2]", "{\"c\":[{}]}"]);
        let mut s = Stream::from_bytes(b"1;2;3");
        let mut n = 0;
        while next_json_record(&mut s, b";", 100).unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
        let mut s = Stream::from_bytes(&vec![b'x'; 500]);
        assert!(next_json_record(&mut s, b"\n", 100).is_err());
    }
}
