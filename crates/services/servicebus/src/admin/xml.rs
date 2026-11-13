//! A tiny XML reader for request bodies: elements, attributes, text. Names are reduced to
//! their local part (`ns0:entry` is `entry`, `xsi:type` is `type`); namespace declarations
//! are dropped. DOCTYPE is refused. Never panics on bad input.

#[derive(Debug, Default, Clone)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

const MAX_DEPTH: usize = 64;

impl Node {
    /// First direct child with this local name.
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Trimmed text content.
    pub fn val(&self) -> &str {
        self.text.trim()
    }

    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == local).map(|(_, v)| v.as_str())
    }
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn decode(s: &str) -> Option<String> {
    if !s.contains('&') {
        return Some(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        let end = after.find(';').filter(|e| *e <= 12)?;
        let ent = &after[..end];
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let num = ent.strip_prefix('#')?;
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(h) => u32::from_str_radix(h, 16).ok()?,
                    None => num.parse::<u32>().ok()?,
                };
                out.push(char::from_u32(code)?);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// End of a start tag (the index of its `>`), skipping quoted attribute values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_start(inner: &str) -> Option<(String, Vec<(String, String)>)> {
    let inner = inner.trim_start();
    let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
    let raw = &inner[..name_end];
    if raw.is_empty() {
        return None;
    }
    let mut rest = inner[name_end..].trim_start();
    let mut attrs = Vec::new();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        rest = rest[eq + 1..].trim_start();
        let q = rest.chars().next()?;
        if q != '"' && q != '\'' {
            return None;
        }
        let close = rest[1..].find(q)? + 1;
        let value = decode(&rest[1..close])?;
        rest = rest[close + 1..].trim_start();
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        attrs.push((local(key).to_string(), value));
    }
    Some((raw.to_string(), attrs))
}

pub fn parse(input: &[u8]) -> Option<Node> {
    let s = std::str::from_utf8(input).ok()?;
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut i = 0;
    let mut stack: Vec<(Node, String)> = Vec::new();
    let mut root: Option<Node> = None;
    while i < s.len() {
        let rest = &s[i..];
        if !rest.starts_with('<') {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = decode(&rest[..end])?;
            match stack.last_mut() {
                Some((n, _)) => n.text.push_str(&text),
                None if text.trim().is_empty() => {}
                None => return None,
            }
            i += end;
        } else if rest.starts_with("<?") {
            i += rest.find("?>")? + 2;
        } else if rest.starts_with("<!--") {
            i += rest.find("-->")? + 3;
        } else if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let e = body.find("]]>")?;
            stack.last_mut()?.0.text.push_str(&body[..e]);
            i += 9 + e + 3;
        } else if rest.starts_with("<!") {
            return None;
        } else if let Some(body) = rest.strip_prefix("</") {
            let e = body.find('>')?;
            let (node, raw) = stack.pop()?;
            if raw != body[..e].trim() {
                return None;
            }
            match stack.last_mut() {
                Some((parent, _)) => parent.children.push(node),
                None => {
                    if root.is_some() {
                        return None;
                    }
                    root = Some(node);
                }
            }
            i += 2 + e + 1;
        } else {
            let end = tag_end(&rest[1..])?;
            let inner = &rest[1..1 + end];
            let (inner, self_closing) = match inner.strip_suffix('/') {
                Some(x) => (x, true),
                None => (inner, false),
            };
            let (raw, attrs) = parse_start(inner)?;
            let node = Node { name: local(&raw).to_string(), attrs, ..Default::default() };
            i += 1 + end + 1;
            if self_closing {
                match stack.last_mut() {
                    Some((parent, _)) => parent.children.push(node),
                    None => {
                        if root.is_some() {
                            return None;
                        }
                        root = Some(node);
                    }
                }
            } else {
                if stack.len() >= MAX_DEPTH {
                    return None;
                }
                stack.push((node, raw));
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdk_body() {
        let body = "<?xml version='1.0' encoding='utf-8'?>\n<ns0:entry xmlns:ns0=\"http://www.w3.org/2005/Atom\" xmlns=\"x\"><ns0:content type=\"application/xml\"><QueueDescription><LockDuration>PT1M</LockDuration><UserMetadata>a &amp; b &#x2713;</UserMetadata><Filter xsi:type=\"SqlFilter\" /></QueueDescription></ns0:content></ns0:entry>";
        let n = parse(body.as_bytes()).unwrap();
        assert_eq!(n.name, "entry");
        let q = n.child("content").unwrap().child("QueueDescription").unwrap();
        assert_eq!(q.child("LockDuration").unwrap().val(), "PT1M");
        assert_eq!(q.child("UserMetadata").unwrap().val(), "a & b \u{2713}");
        assert_eq!(q.child("Filter").unwrap().attr("type"), Some("SqlFilter"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"").is_none());
        assert!(parse(b"<a><b></a>").is_none());
        assert!(parse(b"<a>").is_none());
        assert!(parse(b"<!DOCTYPE a><a/>").is_none());
        assert!(parse(b"text").is_none());
        assert!(parse(b"<a b=1/>").is_none());
        assert!(parse(&[0xff, 0xfe]).is_none());
        assert!(parse(b"<a>&bogus;</a>").is_none());
        let deep = "<a>".repeat(100);
        assert!(parse(deep.as_bytes()).is_none());
    }
}
