//! Blob index tags: Get / Set Blob Tags, Find Blobs by Tags, and the tag filter
//! expression language (`"name" = 'value' AND "other" >= 'a' AND @container = 'c'`).

use std::collections::BTreeMap;

use azemu_core::resp::Resp;

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, snapshot_param, version_param, Cx};
use crate::errors::{blob_not_found, err, ApiError, ApiResult};
use crate::lease::{self, Scope};
use crate::state::Blob;
use crate::util::*;
use crate::xml::{self, Out};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Clone, Debug)]
pub struct Cond {
    pub name: String,
    pub op: Cmp,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct Expr(pub Vec<Cond>);

pub struct EvalCtx<'a> {
    pub tags: &'a BTreeMap<String, String>,
    pub container: &'a str,
}

struct Lexer<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl<'a> Lexer<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn quoted(&mut self, q: u8) -> Result<String, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.i) else { return Err("unterminated string".into()) };
            self.i += 1;
            if c == q {
                if self.s.get(self.i) == Some(&q) {
                    out.push(q);
                    self.i += 1;
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        String::from_utf8(out).map_err(|_| "invalid utf-8".to_string())
    }

    fn name(&mut self) -> Result<String, String> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'"') => self.quoted(b'"'),
            Some(b'@') => {
                let st = self.i;
                self.i += 1;
                while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
                    self.i += 1;
                }
                Ok(self.src[st..self.i].to_string())
            }
            Some(c) if c.is_ascii_alphanumeric() || *c == b'_' => {
                let st = self.i;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_alphanumeric() || matches!(self.s[self.i], b'_' | b'.' | b'-' | b':' | b'/'))
                {
                    self.i += 1;
                }
                Ok(self.src[st..self.i].to_string())
            }
            _ => Err("expected a tag name".into()),
        }
    }

    fn op(&mut self) -> Result<Cmp, String> {
        self.ws();
        let rest = &self.src[self.i..];
        let (op, n) = if rest.starts_with(">=") {
            (Cmp::Ge, 2)
        } else if rest.starts_with("<=") {
            (Cmp::Le, 2)
        } else if rest.starts_with('=') {
            (Cmp::Eq, 1)
        } else if rest.starts_with('>') {
            (Cmp::Gt, 1)
        } else if rest.starts_with('<') && !rest.starts_with("<>") {
            (Cmp::Lt, 1)
        } else {
            return Err("expected a comparison operator".into());
        };
        self.i += n;
        Ok(op)
    }

    fn value(&mut self) -> Result<String, String> {
        self.ws();
        if self.s.get(self.i) == Some(&b'\'') {
            self.quoted(b'\'')
        } else {
            Err("expected a quoted value".into())
        }
    }
}

pub fn parse(src: &str) -> Result<Expr, String> {
    let mut lx = Lexer { s: src.as_bytes(), src, i: 0 };
    let mut conds = vec![];
    loop {
        let name = lx.name()?;
        let op = lx.op()?;
        let value = lx.value()?;
        conds.push(Cond { name, op, value });
        lx.ws();
        if lx.i >= lx.s.len() {
            break;
        }
        let rest = &src[lx.i..];
        if rest.get(..3).map(|w| w.eq_ignore_ascii_case("and")).unwrap_or(false) && rest[3..].starts_with(|c: char| c.is_whitespace()) {
            lx.i += 3;
        } else {
            return Err("expected AND".into());
        }
    }
    Ok(Expr(conds))
}

pub fn holds(e: &Expr, ctx: &EvalCtx<'_>) -> bool {
    e.0.iter().all(|c| {
        let left: &str = if c.name == "@container" {
            ctx.container
        } else {
            match ctx.tags.get(&c.name) {
                Some(v) => v,
                None => return false,
            }
        };
        let ord = left.as_bytes().cmp(c.value.as_bytes());
        match c.op {
            Cmp::Eq => ord.is_eq(),
            Cmp::Gt => ord.is_gt(),
            Cmp::Ge => ord.is_ge(),
            Cmp::Lt => ord.is_lt(),
            Cmp::Le => ord.is_le(),
        }
    })
}

// ---------------------------------------------------------------------------

fn tags_xml(tags: &BTreeMap<String, String>) -> String {
    let mut o = Out::new();
    o.open("Tags").open("TagSet");
    for (k, v) in tags {
        o.open("Tag").el("Key", k).el("Value", v).close("Tag");
    }
    o.close("TagSet").close("Tags");
    o.finish()
}

pub fn get_tags(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;
    cond::check(req, "x-ms-blob-", Some((&b.etag, b.last_modified)), Kind::Read)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    Ok(Resp::new(200u16).xml(tags_xml(&b.tags)).build())
}

pub fn set_tags(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let version = version_param(req)?;
    let text = std::str::from_utf8(&req.body).map_err(|_| ApiError::invalid_xml("The body is not valid UTF-8."))?;
    let root = xml::parse(text).map_err(|e| ApiError::invalid_xml(&e))?;
    if root.name != "Tags" {
        return Err(ApiError::invalid_xml("The root element must be Tags."));
    }
    let mut tags = BTreeMap::new();
    if let Some(ts) = root.child("TagSet") {
        for t in ts.children_named("Tag") {
            let k = t.child_text("Key").ok_or_else(|| err("MissingRequiredXmlNode").detail("XmlNodeName", "Key"))?;
            let v = t.child_text("Value").unwrap_or("");
            if tags.insert(k.to_string(), v.to_string()).is_some() {
                return Err(ApiError::invalid_xml("Duplicate tag key."));
            }
        }
    }
    validate_tags(&tags)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find_mut(None, version.as_deref()).ok_or_else(blob_not_found)?;
    cond::check(req, "x-ms-blob-", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    b.tags = tags;
    Ok(Resp::new(204u16).build())
}

/// Find Blobs by Tags, account wide (`container` None) or inside one container.
pub fn find_blobs(cx: &mut Cx, container: Option<&str>) -> ApiResult {
    let req = cx.req;
    let where_clause = req
        .query
        .get("where")
        .ok_or_else(|| ApiError::new("MissingRequiredQueryParameter").detail("QueryParameterName", "where"))?
        .to_string();
    let expr = parse(&where_clause).map_err(|_| ApiError::invalid_query("where", &where_clause))?;
    let max = crate::list::max_results(req)?;
    let marker = req.query.get("marker").unwrap_or("").to_string();
    let resume: Option<(String, String)> = if marker.is_empty() {
        None
    } else {
        let bad = || ApiError::invalid_query("marker", &marker);
        let raw = String::from_utf8(b64_decode(&marker).ok_or_else(bad)?).map_err(|_| bad())?;
        let (c, b) = raw.split_once('\0').ok_or_else(bad)?;
        Some((c.to_string(), b.to_string()))
    };
    let endpoint = crate::list::service_endpoint(req);
    let acct = cx.account();
    if let Some(c) = container {
        if !acct.containers.contains_key(c) {
            return Err(crate::errors::container_not_found());
        }
    }
    let mut hits: Vec<(&str, &str, &Blob)> = vec![];
    for (cname, c) in &acct.containers {
        if container.map(|n| n != cname).unwrap_or(false) {
            continue;
        }
        for (bname, e) in &c.blobs {
            if let Some(b) = e.live() {
                if b.tags.is_empty() {
                    continue;
                }
                if let Some((rc, rb)) = &resume {
                    if (cname.as_str(), bname.as_str()) < (rc.as_str(), rb.as_str()) {
                        continue;
                    }
                }
                if holds(&expr, &EvalCtx { tags: &b.tags, container: cname }) {
                    hits.push((cname, bname, b));
                }
            }
        }
    }
    let next = hits.get(max).map(|(c, b, _)| b64(format!("{c}\0{b}").as_bytes()));
    let mut o = Out::new();
    o.open_attrs("EnumerationResults", &[("ServiceEndpoint", &endpoint)]);
    o.el("Where", &where_clause);
    if req.query.has("maxresults") {
        o.el("MaxResults", &max.to_string());
    }
    o.open("Blobs");
    for (c, b, blob) in hits.iter().take(max) {
        o.open("Blob").el("Name", b).el("ContainerName", c);
        o.open("Tags").open("TagSet");
        for (k, v) in &blob.tags {
            o.open("Tag").el("Key", k).el("Value", v).close("Tag");
        }
        o.close("TagSet").close("Tags").close("Blob");
    }
    o.close("Blobs");
    o.el("NextMarker", next.as_deref().unwrap_or(""));
    o.close("EnumerationResults");
    Ok(Resp::new(200u16).xml(o.finish()).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parses_and_evaluates() {
        let e = parse("\"project\"='abc' AND \"rank\">='a' and \"rank\"<'z' AND @container='c1'").unwrap();
        assert_eq!(e.0.len(), 4);
        let t = tags(&[("project", "abc"), ("rank", "m")]);
        assert!(holds(&e, &EvalCtx { tags: &t, container: "c1" }));
        assert!(!holds(&e, &EvalCtx { tags: &t, container: "c2" }));
        let e = parse("tag1='it''s'").unwrap();
        assert_eq!(e.0[0].value, "it's");
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(parse("\"a\"").is_err());
        assert!(parse("\"a\"=b").is_err());
        assert!(parse("\"a\"='b' OR \"c\"='d'").is_err());
        assert!(parse("").is_err());
    }
}
