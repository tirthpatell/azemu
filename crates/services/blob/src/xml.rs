//! A small XML writer and a small XML parser. Request bodies here are tiny
//! (block lists, service properties, tag sets), so a tree parser is enough.

use azemu_core::storage::xml_escape;

/// XML document text builder.
pub struct Out {
    pub s: String,
}

impl Out {
    /// Starts with the BOM and the XML declaration, as Azure does.
    pub fn new() -> Out {
        Out { s: String::from("\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>") }
    }

    pub fn open(&mut self, tag: &str) -> &mut Out {
        self.s.push('<');
        self.s.push_str(tag);
        self.s.push('>');
        self
    }

    pub fn open_attrs(&mut self, tag: &str, attrs: &[(&str, &str)]) -> &mut Out {
        self.s.push('<');
        self.s.push_str(tag);
        for (k, v) in attrs {
            self.s.push(' ');
            self.s.push_str(k);
            self.s.push_str("=\"");
            self.s.push_str(&xml_escape(v));
            self.s.push('"');
        }
        self.s.push('>');
        self
    }

    pub fn close(&mut self, tag: &str) -> &mut Out {
        self.s.push_str("</");
        self.s.push_str(tag);
        self.s.push('>');
        self
    }

    /// `<Tag>text</Tag>`, or `<Tag />` when the text is empty.
    pub fn el(&mut self, tag: &str, text: &str) -> &mut Out {
        if text.is_empty() {
            self.s.push('<');
            self.s.push_str(tag);
            self.s.push_str(" />");
        } else {
            self.open(tag);
            self.s.push_str(&xml_escape(text));
            self.close(tag);
        }
        self
    }

    /// Element only when there is a value.
    pub fn opt(&mut self, tag: &str, text: Option<&str>) -> &mut Out {
        if let Some(t) = text {
            self.el(tag, t);
        }
        self
    }

    pub fn empty(&mut self, tag: &str) -> &mut Out {
        self.s.push('<');
        self.s.push_str(tag);
        self.s.push_str(" />");
        self
    }

    pub fn raw(&mut self, text: &str) -> &mut Out {
        self.s.push_str(text);
        self
    }

    pub fn finish(self) -> String {
        self.s
    }
}

impl Default for Out {
    fn default() -> Self {
        Out::new()
    }
}

/// Parsed element.
#[derive(Debug, Default, Clone)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
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

/// Elements nested deeper than this are refused. Every real request body here is at most
/// five levels deep; the parser calls itself once per level, so without a limit a body of
/// `<a><a><a>...` could use up the stack and kill the process.
pub const MAX_DEPTH: usize = 64;

/// Documents larger than this are refused (a block list of 50,000 blocks is about 7 MB).
pub const MAX_BYTES: usize = 16 * 1024 * 1024;

/// Parse a document. Err carries a short reason.
pub fn parse(input: &str) -> Result<Node, String> {
    if input.len() > MAX_BYTES {
        return Err(format!("The XML document is larger than {MAX_BYTES} bytes."));
    }
    let s = input.strip_prefix('\u{feff}').unwrap_or(input);
    let b = s.as_bytes();
    let mut p = Parser { s, b, i: 0, depth: 0 };
    p.skip_misc()?;
    let root = p.element()?;
    p.skip_misc()?;
    if p.i < b.len() {
        return Err("data after the root element".into());
    }
    Ok(root)
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    /// How many elements are open right now.
    depth: usize,
}

impl<'a> Parser<'a> {
    fn starts(&self, pat: &str) -> bool {
        self.s[self.i..].starts_with(pat)
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn skip_until(&mut self, pat: &str) -> Result<(), String> {
        match self.s[self.i..].find(pat) {
            Some(n) => {
                self.i += n + pat.len();
                Ok(())
            }
            None => Err(format!("unterminated construct, expected {pat}")),
        }
    }

    /// Whitespace, declaration, comments, doctype.
    fn skip_misc(&mut self) -> Result<(), String> {
        loop {
            self.skip_ws();
            if self.starts("<?") {
                self.skip_until("?>")?;
            } else if self.starts("<!--") {
                self.skip_until("-->")?;
            } else if self.starts("<!DOCTYPE") {
                self.skip_until(">")?;
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let st = self.i;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':') || c >= 0x80 {
                self.i += 1;
            } else {
                break;
            }
        }
        if st == self.i {
            return Err("expected a name".into());
        }
        Ok(self.s[st..self.i].to_string())
    }

    fn element(&mut self) -> Result<Node, String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!("Elements are nested more than {MAX_DEPTH} levels deep."));
        }
        self.depth += 1;
        let r = self.element_inner();
        self.depth -= 1;
        r
    }

    fn element_inner(&mut self) -> Result<Node, String> {
        if self.i >= self.b.len() || self.b[self.i] != b'<' {
            return Err("expected an element".into());
        }
        self.i += 1;
        let mut node = Node { name: self.name()?, ..Node::default() };
        loop {
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err("unexpected end of document".into());
            }
            match self.b[self.i] {
                b'/' => {
                    if !self.starts("/>") {
                        return Err("malformed empty element".into());
                    }
                    self.i += 2;
                    return Ok(node);
                }
                b'>' => {
                    self.i += 1;
                    break;
                }
                _ => {
                    let k = self.name()?;
                    self.skip_ws();
                    if self.i >= self.b.len() || self.b[self.i] != b'=' {
                        return Err("expected = in attribute".into());
                    }
                    self.i += 1;
                    self.skip_ws();
                    let q = *self.b.get(self.i).ok_or("unexpected end")?;
                    if q != b'"' && q != b'\'' {
                        return Err("expected quote in attribute".into());
                    }
                    self.i += 1;
                    let st = self.i;
                    while self.i < self.b.len() && self.b[self.i] != q {
                        self.i += 1;
                    }
                    if self.i >= self.b.len() {
                        return Err("unterminated attribute".into());
                    }
                    let v = unescape(&self.s[st..self.i])?;
                    self.i += 1;
                    node.attrs.push((k, v));
                }
            }
        }
        // content
        loop {
            if self.i >= self.b.len() {
                return Err("unexpected end of document".into());
            }
            if self.starts("</") {
                self.i += 2;
                let n = self.name()?;
                if n != node.name {
                    return Err(format!("mismatched end tag {n}, expected {}", node.name));
                }
                self.skip_ws();
                if self.i >= self.b.len() || self.b[self.i] != b'>' {
                    return Err("malformed end tag".into());
                }
                self.i += 1;
                return Ok(node);
            } else if self.starts("<!--") {
                self.skip_until("-->")?;
            } else if self.starts("<![CDATA[") {
                self.i += 9;
                let st = self.i;
                self.skip_until("]]>")?;
                node.text.push_str(&self.s[st..self.i - 3]);
            } else if self.starts("<?") {
                self.skip_until("?>")?;
            } else if self.b[self.i] == b'<' {
                let child = self.element()?;
                node.children.push(child);
            } else {
                let st = self.i;
                while self.i < self.b.len() && self.b[self.i] != b'<' {
                    self.i += 1;
                }
                node.text.push_str(&unescape(&self.s[st..self.i])?);
            }
        }
    }
}

fn unescape(s: &str) -> Result<String, String> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p + 1..];
        let end = rest.find(';').ok_or("unterminated entity")?;
        let ent = &rest[..end];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                let n = u32::from_str_radix(&ent[2..], 16).map_err(|_| "bad character reference")?;
                out.push(char::from_u32(n).ok_or("bad character reference")?);
            }
            _ if ent.starts_with('#') => {
                let n: u32 = ent[1..].parse().map_err(|_| "bad character reference")?;
                out.push(char::from_u32(n).ok_or("bad character reference")?);
            }
            _ => return Err(format!("unknown entity &{ent};")),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_list() {
        let n = parse("\u{feff}<?xml version='1.0' encoding='utf-8'?>\n<BlockList><Latest>YWJj</Latest><Committed>a&amp;b</Committed><Empty/></BlockList>").unwrap();
        assert_eq!(n.name, "BlockList");
        assert_eq!(n.children.len(), 3);
        assert_eq!(n.child_text("Committed"), Some("a&b"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("not xml").is_err());
        assert!(parse("<a>").is_err());
    }

    #[test]
    fn refuses_deep_nesting_without_overflowing_the_stack() {
        let n = 100_000;
        let doc = format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
        let err = parse(&doc).unwrap_err();
        assert!(err.contains("nested"), "{err}");
        // An unterminated one fails the same way, long before the end of the input.
        assert!(parse(&"<a>".repeat(n)).unwrap_err().contains("nested"));
        // Just inside the limit still parses.
        let ok = format!("{}{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(parse(&ok).is_ok());
        let too = format!("{}{}", "<a>".repeat(MAX_DEPTH + 1), "</a>".repeat(MAX_DEPTH + 1));
        assert!(parse(&too).is_err());
    }

    #[test]
    fn refuses_huge_documents() {
        let doc = format!("<a>{}</a>", "x".repeat(MAX_BYTES));
        assert!(parse(&doc).is_err());
    }

    #[test]
    fn writer_escapes() {
        let mut o = Out::new();
        o.el("A", "x<y").el("B", "");
        assert!(o.s.ends_with("<A>x&lt;y</A><B />"));
    }
}
