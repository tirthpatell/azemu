//! A small XML reader for request bodies and helpers for writing responses.
//!
//! Queue bodies are tiny and have a fixed shape, so this reads the whole document
//! into a tree of [`Node`]s. It rejects what Azure rejects: characters that XML 1.0
//! does not allow (for example `\u{1}`), unbalanced tags, bad entity references and a
//! DOCTYPE. It reports the line and column of the problem, which Azure puts into
//! the `InvalidXmlDocument` error body.

use std::cell::Cell;

use azemu_core::storage::xml_escape;

/// Where something is in the document. Both numbers start at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// One element: its direct text and its child elements.
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    /// Direct text (and CDATA) with entities decoded, not trimmed.
    pub text: String,
    pub children: Vec<Node>,
    /// Position of the element name in the end tag (or in the start tag for `<a/>`).
    pub end: Pos,
    /// Position of the first character after the element.
    pub after: Pos,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct XmlError {
    pub pos: Pos,
    pub reason: String,
}

const MAX_DEPTH: usize = 32;

/// Read a document and return its root element.
pub fn parse(input: &str) -> Result<Node, XmlError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut p = Parser { s: input.as_bytes(), text: input, i: 0, cursor: Cell::new((0, 1, 1)) };
    p.check_chars()?;
    p.skip_misc()?;
    if p.i >= p.s.len() {
        return Err(p.err_here("Error parsing Xml content"));
    }
    let root = p.element(0)?;
    p.skip_misc()?;
    if p.i < p.s.len() {
        return Err(p.err_here("Error parsing Xml content"));
    }
    Ok(root)
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
    /// (byte offset, line, column) of the last position that was asked for.
    cursor: Cell<(usize, usize, usize)>,
}

fn valid_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || ('\u{20}'..='\u{d7ff}').contains(&c) || ('\u{e000}'..='\u{fffd}').contains(&c) || c >= '\u{10000}'
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':' || b >= 0x80
}

fn is_name_char(b: u8) -> bool {
    is_name_start(b) || b.is_ascii_digit() || b == b'-' || b == b'.'
}

impl<'a> Parser<'a> {
    /// Line and column of a byte offset. Offsets asked for while parsing only move
    /// forward, so the scan resumes from the last answer instead of the start.
    fn pos_at(&self, offset: usize) -> Pos {
        let (mut at, mut line, mut col) = self.cursor.get();
        if offset < at {
            (at, line, col) = (0, 1, 1);
        }
        for c in self.text[at..].chars() {
            if at >= offset {
                break;
            }
            at += c.len_utf8();
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        self.cursor.set((at, line, col));
        Pos { line, col }
    }

    fn err_at(&self, offset: usize, reason: &str) -> XmlError {
        XmlError { pos: self.pos_at(offset), reason: reason.to_string() }
    }

    fn err_here(&self, reason: &str) -> XmlError {
        self.err_at(self.i.min(self.s.len()), reason)
    }

    /// First character that XML 1.0 does not allow anywhere in a document.
    fn check_chars(&self) -> Result<(), XmlError> {
        for (idx, c) in self.text.char_indices() {
            if !valid_xml_char(c) {
                return Err(self.err_at(idx, "Error parsing Xml content"));
            }
        }
        Ok(())
    }

    fn starts_with(&self, lit: &str) -> bool {
        self.s[self.i..].starts_with(lit.as_bytes())
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    /// Skip whitespace, comments and processing instructions (including `<?xml ?>`).
    fn skip_misc(&mut self) -> Result<(), XmlError> {
        loop {
            self.skip_ws();
            if self.starts_with("<?") {
                match find(self.s, self.i + 2, b"?>") {
                    Some(end) => self.i = end + 2,
                    None => return Err(self.err_here("Error parsing Xml content")),
                }
            } else if self.starts_with("<!--") {
                match find(self.s, self.i + 4, b"-->") {
                    Some(end) => self.i = end + 3,
                    None => return Err(self.err_here("Error parsing Xml content")),
                }
            } else if self.starts_with("<!") {
                return Err(self.err_here("DTD is prohibited"));
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<String, XmlError> {
        let start = self.i;
        if self.i >= self.s.len() || !is_name_start(self.s[self.i]) {
            return Err(self.err_here("Error parsing Xml content"));
        }
        while self.i < self.s.len() && is_name_char(self.s[self.i]) {
            self.i += 1;
        }
        Ok(self.text[start..self.i].to_string())
    }

    fn element(&mut self, depth: usize) -> Result<Node, XmlError> {
        if depth > MAX_DEPTH {
            return Err(self.err_here("Error parsing Xml content"));
        }
        if self.i >= self.s.len() || self.s[self.i] != b'<' {
            return Err(self.err_here("Error parsing Xml content"));
        }
        self.i += 1;
        let name_pos = self.i;
        let name = self.name()?;
        // attributes: parsed for well-formedness and ignored
        loop {
            let before = self.i;
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b'/') => {
                    if self.s.get(self.i + 1) != Some(&b'>') {
                        return Err(self.err_here("Error parsing Xml content"));
                    }
                    self.i += 2;
                    let end = self.pos_at(name_pos);
                    let after = self.pos_at(self.i);
                    return Ok(Node { name, text: String::new(), children: vec![], end, after });
                }
                Some(b'>') => {
                    self.i += 1;
                    break;
                }
                Some(_) if self.i > before => {
                    self.name()?;
                    self.skip_ws();
                    if self.s.get(self.i) != Some(&b'=') {
                        return Err(self.err_here("Error parsing Xml content"));
                    }
                    self.i += 1;
                    self.skip_ws();
                    let quote = match self.s.get(self.i) {
                        Some(q @ (b'"' | b'\'')) => *q,
                        _ => return Err(self.err_here("Error parsing Xml content")),
                    };
                    self.i += 1;
                    let start = self.i;
                    while self.i < self.s.len() && self.s[self.i] != quote {
                        if self.s[self.i] == b'<' {
                            return Err(self.err_here("Error parsing Xml content"));
                        }
                        self.i += 1;
                    }
                    if self.i >= self.s.len() {
                        return Err(self.err_here("Error parsing Xml content"));
                    }
                    let raw = &self.text[start..self.i];
                    self.decode_entities(raw, start)?;
                    self.i += 1;
                }
                _ => return Err(self.err_here("Error parsing Xml content")),
            }
        }

        let mut node = Node { name: name.clone(), ..Node::default() };
        loop {
            if self.i >= self.s.len() {
                return Err(self.err_here("Error parsing Xml content"));
            }
            if self.starts_with("</") {
                self.i += 2;
                let end_name_at = self.i;
                let close = self.name()?;
                if close != name {
                    return Err(self.err_at(end_name_at, "Error parsing Xml content"));
                }
                self.skip_ws();
                if self.s.get(self.i) != Some(&b'>') {
                    return Err(self.err_here("Error parsing Xml content"));
                }
                self.i += 1;
                node.end = self.pos_at(end_name_at);
                node.after = self.pos_at(self.i);
                return Ok(node);
            } else if self.starts_with("<!--") {
                match find(self.s, self.i + 4, b"-->") {
                    Some(end) => self.i = end + 3,
                    None => return Err(self.err_here("Error parsing Xml content")),
                }
            } else if self.starts_with("<![CDATA[") {
                let start = self.i + 9;
                match find(self.s, start, b"]]>") {
                    Some(end) => {
                        node.text.push_str(&self.text[start..end]);
                        self.i = end + 3;
                    }
                    None => return Err(self.err_here("Error parsing Xml content")),
                }
            } else if self.starts_with("<?") {
                match find(self.s, self.i + 2, b"?>") {
                    Some(end) => self.i = end + 2,
                    None => return Err(self.err_here("Error parsing Xml content")),
                }
            } else if self.s[self.i] == b'<' {
                let child = self.element(depth + 1)?;
                node.children.push(child);
            } else {
                let start = self.i;
                while self.i < self.s.len() && self.s[self.i] != b'<' {
                    self.i += 1;
                }
                let raw = &self.text[start..self.i];
                let decoded = self.decode_entities(raw, start)?;
                node.text.push_str(&decoded);
            }
        }
    }

    fn decode_entities(&self, raw: &str, base: usize) -> Result<String, XmlError> {
        if !raw.contains(['&', '\r']) {
            return Ok(raw.to_string());
        }
        let mut out = String::with_capacity(raw.len());
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'&' => {
                    let Some(semi) = raw[i..].find(';') else {
                        return Err(self.err_at(base + i, "Error parsing Xml content"));
                    };
                    let ent = &raw[i + 1..i + semi];
                    let ch = match ent {
                        "lt" => '<',
                        "gt" => '>',
                        "amp" => '&',
                        "quot" => '"',
                        "apos" => '\'',
                        _ => {
                            let code = if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                                u32::from_str_radix(hex, 16).ok()
                            } else if let Some(dec) = ent.strip_prefix('#') {
                                dec.parse::<u32>().ok()
                            } else {
                                None
                            };
                            match code.and_then(char::from_u32).filter(|c| valid_xml_char(*c)) {
                                Some(c) => c,
                                None => return Err(self.err_at(base + i, "Error parsing Xml content")),
                            }
                        }
                    };
                    out.push(ch);
                    i += semi + 1;
                }
                b'\r' => {
                    // line ends are normalised to \n
                    out.push('\n');
                    i += if bytes.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                }
                _ => {
                    let next = raw[i..].find(['&', '\r']).map(|n| i + n).unwrap_or(raw.len());
                    out.push_str(&raw[i..next]);
                    i = next;
                }
            }
        }
        Ok(out)
    }
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// `<?xml ...?>` header with the byte order mark Azure sends.
pub const PROLOG: &str = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>";

/// A document with the standard prolog.
pub fn document(body: &str) -> String {
    format!("{PROLOG}{body}")
}

/// `<name>escaped</name>`
pub fn element(name: &str, value: &str) -> String {
    if value.is_empty() {
        return format!("<{name} />");
    }
    format!("<{name}>{}</{name}>", xml_escape(value))
}

/// Text for a message body in a response. Escapes the five XML characters and keeps
/// everything else, so what was stored comes back unchanged.
pub fn text(value: &str) -> String {
    xml_escape(value).into_owned()
}

/// Parse a boolean the way an XML schema does.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_message_with_declaration() {
        let n = parse(
            "<?xml version='1.0' encoding='utf-8'?>\n<QueueMessage><MessageText> a &lt;b&gt; &amp; &#x41;\n</MessageText></QueueMessage>",
        )
        .unwrap();
        assert_eq!(n.name, "QueueMessage");
        assert_eq!(n.child_text("MessageText"), Some(" a <b> & A\n"));
    }

    #[test]
    fn control_character_position_matches_azure() {
        // recording test_message_text_xml_invalid_chars: line 2, position 28
        let e = parse("<?xml version='1.0' encoding='utf-8'?>\n<QueueMessage><MessageText>\u{1}</MessageText></QueueMessage>").unwrap_err();
        assert_eq!((e.pos.line, e.pos.col), (2, 28));
    }

    #[test]
    fn end_tag_position() {
        let body = "<?xml version='1.0' encoding='utf-8'?>\n<StorageServiceProperties><MinuteMetrics><Version>1.0</Version><Enabled>true</Enabled><IncludeAPIs>true</IncludeAPIs><RetentionPolicy><Enabled>true</Enabled><Days>366</Days></RetentionPolicy></MinuteMetrics></StorageServiceProperties>";
        let n = parse(body).unwrap();
        let rp = n.child("MinuteMetrics").unwrap().child("RetentionPolicy").unwrap();
        assert_eq!((rp.end.line, rp.end.col), (2, 176));
    }

    #[test]
    fn position_after_element() {
        // recording (async) retention_too_long: an empty root, error at line 2, position 29
        let n = parse("<?xml version='1.0' encoding='utf-8'?>\n<StorageServiceProperties />").unwrap();
        assert_eq!((n.after.line, n.after.col), (2, 29));
    }

    #[test]
    fn rejects_broken_documents() {
        for bad in
            ["", "<a>", "<a></b>", "<a>&bogus;</a>", "<a>&#1;</a>", "<a/><b/>", "<!DOCTYPE a><a/>", "<a b=c/>", "text", "<a>\u{fffe}</a>"]
        {
            assert!(parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn empty_elements_and_cdata() {
        let n = parse("<a><b/><c><![CDATA[<x>]]></c><!-- hi --></a>").unwrap();
        assert_eq!(n.children.len(), 2);
        assert_eq!(n.child_text("c"), Some("<x>"));
        assert_eq!(n.child_text("b"), Some(""));
    }

    #[test]
    fn never_panics_on_garbage() {
        let alphabet = [
            "<",
            ">",
            "/",
            "a",
            "b",
            "&",
            ";",
            "é",
            "\u{1f680}",
            "!",
            "-",
            "[",
            "CDATA",
            "?",
            "=",
            "\"",
            "'",
            " ",
            "\n",
            "\r",
            "#",
            "x",
            "<![CDATA[",
            "]]>",
            "<!--",
            "-->",
            "&amp;",
            "&#",
            "\u{0}",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            let mut doc = String::new();
            for _ in 0..(seed % 24) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                doc.push_str(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let _ = parse(&doc);
        }
    }
}
