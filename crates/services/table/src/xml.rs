//! A tiny XML reader for the two request bodies the service accepts (stored
//! access policies and service properties). It builds an element tree and
//! ignores namespaces, attributes, comments and processing instructions.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }
}

const MAX_DEPTH: usize = 64;

/// Parse a document and return its root element, or `Err(())` when it is not
/// well formed.
pub fn parse(input: &str) -> Result<Node, ()> {
    let s = input.strip_prefix('\u{feff}').unwrap_or(input);
    let b = s.as_bytes();
    let mut pos = 0usize;
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    while pos < b.len() {
        if b[pos] == b'<' {
            if s[pos..].starts_with("<?") {
                pos += s[pos..].find("?>").ok_or(())? + 2;
            } else if s[pos..].starts_with("<!--") {
                pos += s[pos..].find("-->").ok_or(())? + 3;
            } else if s[pos..].starts_with("<![CDATA[") {
                let end = s[pos..].find("]]>").ok_or(())?;
                let text = &s[pos + 9..pos + end];
                stack.last_mut().ok_or(())?.text.push_str(text);
                pos += end + 3;
            } else if s[pos..].starts_with("<!") {
                pos += s[pos..].find('>').ok_or(())? + 1;
            } else if s[pos..].starts_with("</") {
                let end = s[pos..].find('>').ok_or(())?;
                let name = local(s[pos + 2..pos + end].trim());
                let node = stack.pop().ok_or(())?;
                if node.name != name {
                    return Err(());
                }
                pos += end + 1;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => {
                        if root.is_some() {
                            return Err(());
                        }
                        root = Some(node);
                    }
                }
            } else {
                let end = tag_end(s, pos).ok_or(())?;
                let mut inner = &s[pos + 1..end];
                let self_closing = inner.ends_with('/');
                if self_closing {
                    inner = &inner[..inner.len() - 1];
                }
                let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
                let name = local(&inner[..name_end]);
                if name.is_empty() {
                    return Err(());
                }
                let node = Node { name, ..Default::default() };
                if stack.len() >= MAX_DEPTH {
                    return Err(());
                }
                pos = end + 1;
                if self_closing {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => {
                            if root.is_some() {
                                return Err(());
                            }
                            root = Some(node);
                        }
                    }
                } else {
                    stack.push(node);
                }
            }
        } else {
            let next = s[pos..].find('<').map(|i| pos + i).unwrap_or(b.len());
            let raw = &s[pos..next];
            match stack.last_mut() {
                Some(n) => n.text.push_str(&unescape(raw)?),
                None => {
                    if !raw.trim().is_empty() {
                        return Err(());
                    }
                }
            }
            pos = next;
        }
    }
    if !stack.is_empty() {
        return Err(());
    }
    root.ok_or(())
}

/// Index of the `>` that closes the tag starting at `start`, skipping quoted attribute values.
fn tag_end(s: &str, start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, &c) in s.as_bytes().iter().enumerate().skip(start + 1) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

fn unescape(raw: &str) -> Result<String, ()> {
    if !raw.contains('&') {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let semi = rest.find(';').ok_or(())?;
        let ent = &rest[1..semi];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).map_err(|_| ())?
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse::<u32>().map_err(|_| ())?
                } else {
                    return Err(());
                };
                out.push(char::from_u32(code).ok_or(())?);
            }
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Escape text for an element body or an attribute value.
pub fn escape(s: &str) -> String {
    azemu_core::storage::xml_escape(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_elements_and_entities() {
        let n = parse("<?xml version='1.0'?>\n<A x=\"1>2\"><B>a &amp; b</B><C/><D><![CDATA[<x>]]></D></A>").unwrap();
        assert_eq!(n.name, "A");
        assert_eq!(n.child_text("B"), Some("a & b"));
        assert!(n.child("C").is_some());
        assert_eq!(n.child_text("D"), Some("<x>"));
    }

    #[test]
    fn namespaces_are_dropped() {
        let n = parse("<m:error xmlns:m=\"u\"><m:code>X</m:code></m:error>").unwrap();
        assert_eq!(n.name, "error");
        assert_eq!(n.child_text("code"), Some("X"));
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "<a>", "<a></b>", "<a></a><b></b>", "text", "<a>&bogus;</a>", "<a"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bom_is_skipped() {
        assert!(parse("\u{feff}<a/>").is_ok());
    }

    #[test]
    fn nesting_is_limited() {
        let deep = format!("{}{}", "<a>".repeat(10_000), "</a>".repeat(10_000));
        assert!(parse(&deep).is_err());
        let ok = format!("{}{}", "<a>".repeat(60), "</a>".repeat(60));
        assert!(parse(&ok).is_ok());
    }
}
