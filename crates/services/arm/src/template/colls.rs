//! Array, object, string and number functions that the evaluator in `template.rs` does not need to know the inside of.
//! Each one is described on one of the pages of "ARM template functions" (Microsoft Learn,
//! `azure-resource-manager/templates/template-functions-array`, `-object`, `-string`, `-numeric`, `-logical`); the
//! page is named where a function starts.

use base64::Engine;
use serde_json::{json, Map, Value};

use super::{eq_values, err, invalid, s, EvalError};
use crate::limits;

type R = Result<Value, EvalError>;

fn need(name: &str, v: &[Value], n: usize) -> Result<(), EvalError> {
    if v.len() < n {
        err(format!("The function '{name}' needs at least {n} argument(s)."))
    } else {
        Ok(())
    }
}

/// Equality that tells `5` from `'5'` and `'a'` from `'A'` (array functions: "case-sensitive for strings").
fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| strict_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map(|w| strict_eq(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

fn int_arg(v: &Value, what: &str) -> Result<i64, EvalError> {
    match v {
        Value::Number(n) => n.as_i64().ok_or_else(|| EvalError::Msg(format!("{what} must be an integer."))),
        Value::String(t) => t.trim().parse().map_err(|_| EvalError::Msg(format!("{what} must be an integer."))),
        _ => err(format!("{what} must be an integer.")),
    }
}

/// `Some(result)` when `lname` is one of the functions in this file.
pub fn call(name: &str, lname: &str, v: &[Value]) -> Option<R> {
    Some(match lname {
        // ---- logical, numeric: https://learn.microsoft.com/azure/azure-resource-manager/templates/template-functions-logical
        "true" => Ok(json!(true)),
        "false" => Ok(json!(false)),
        // template-functions-object#null
        "null" => Ok(Value::Null),
        // template-functions-numeric#float
        "float" => need(name, v, 1).and_then(|_| match &v[0] {
            Value::Number(n) => Ok(json!(n.as_f64().unwrap_or(0.0))),
            Value::String(t) => t
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(|f| json!(f))
                .ok_or_else(|| EvalError::Msg(format!("float: '{t}' is not a number."))),
            _ => err("float: a number or a string was expected."),
        }),
        // ---- template-functions-array
        "array" => need(name, v, 1).map(|_| match &v[0] {
            Value::Array(_) => v[0].clone(),
            other => Value::Array(vec![other.clone()]),
        }),
        "distinct" => need(name, v, 1).and_then(|_| match &v[0] {
            Value::Array(a) => {
                let mut seen = std::collections::HashSet::new();
                Ok(Value::Array(a.iter().filter(|x| seen.insert(limits::canonical(x))).cloned().collect()))
            }
            _ => err("distinct: an array was expected."),
        }),
        "indexof" => need(name, v, 2).map(|_| match &v[0] {
            Value::Array(a) => json!(a.iter().position(|x| strict_eq(x, &v[1])).map(|i| i as i64).unwrap_or(-1)),
            // string form: case-insensitive, template-functions-string#indexof
            other => {
                let text = s(other);
                json!(text.to_lowercase().find(&s(&v[1]).to_lowercase()).map(|i| char_at_lowered_byte(&text, i)).unwrap_or(-1))
            }
        }),
        "lastindexof" => need(name, v, 2).map(|_| match &v[0] {
            Value::Array(a) => json!(a.iter().rposition(|x| strict_eq(x, &v[1])).map(|i| i as i64).unwrap_or(-1)),
            other => {
                let text = s(other);
                json!(text.to_lowercase().rfind(&s(&v[1]).to_lowercase()).map(|i| char_at_lowered_byte(&text, i)).unwrap_or(-1))
            }
        }),
        "indexfromend" | "tryindexfromend" => need(name, v, 2).and_then(|_| {
            let Value::Array(a) = &v[0] else { return err(format!("{name}: an array was expected.")) };
            let n = int_arg(&v[1], "the reverse index")?;
            let found = if n >= 1 && (n as u64) <= a.len() as u64 { a.get(a.len() - n as usize).cloned() } else { None };
            match (found, lname) {
                (Some(x), _) => Ok(x),
                (None, "tryindexfromend") => Ok(Value::Null),
                (None, _) => err(format!("indexFromEnd: the reverse index {n} is out of range for an array of {} element(s).", a.len())),
            }
        }),
        "trygetitem" | "tryget" => need(name, v, 2).map(|_| match (&v[0], &v[1]) {
            (Value::Object(o), key) => {
                let k = s(key);
                o.iter().find(|(x, _)| x.eq_ignore_ascii_case(&k)).map(|(_, x)| x.clone()).unwrap_or(Value::Null)
            }
            (Value::Array(a), Value::Number(n)) => {
                n.as_i64().and_then(|n| usize::try_from(n).ok()).and_then(|n| a.get(n)).cloned().unwrap_or(Value::Null)
            }
            _ => Value::Null,
        }),
        "intersection" => need(name, v, 2).and_then(|_| {
            if v.iter().all(Value::is_array) {
                let first = v[0].as_array().cloned().unwrap_or_default();
                let others: Vec<std::collections::HashSet<String>> =
                    v[1..].iter().map(|x| x.as_array().into_iter().flatten().map(limits::canonical).collect()).collect();
                let mut seen = std::collections::HashSet::new();
                Ok(Value::Array(
                    first
                        .into_iter()
                        .filter(|x| {
                            let k = limits::canonical(x);
                            others.iter().all(|o| o.contains(&k)) && seen.insert(k)
                        })
                        .collect(),
                ))
            } else if v.iter().all(Value::is_object) {
                let mut out = Map::new();
                if let Some(first) = v[0].as_object() {
                    for (k, x) in first {
                        let everywhere =
                            v[1..].iter().all(|o| o.as_object().and_then(|o| o.get(k)).map(|y| eq_values(x, y)).unwrap_or(false));
                        if everywhere {
                            out.insert(k.clone(), x.clone());
                        }
                    }
                }
                Ok(Value::Object(out))
            } else {
                err("intersection: every argument must be an array, or every argument must be an object.")
            }
        }),
        "flatten" => need(name, v, 1).and_then(|_| match &v[0] {
            Value::Array(a) => Ok(Value::Array(a.iter().flat_map(|x| x.as_array().cloned().unwrap_or_else(|| vec![x.clone()])).collect())),
            _ => err("flatten: an array of arrays was expected."),
        }),
        // ---- template-functions-object
        "items" => need(name, v, 1).and_then(|_| match &v[0] {
            Value::Object(o) => {
                // "the Bicep items() function sorts the objects in the alphabetical order" (the page's remark on object ordering)
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
                Ok(Value::Array(keys.into_iter().map(|k| json!({"key": k, "value": o[k]})).collect()))
            }
            _ => err("items: an object was expected."),
        }),
        "objectkeys" => need(name, v, 1).and_then(|_| match &v[0] {
            Value::Object(o) => {
                // Azure answers the keys in alphabetical order, like `items` (checked against a real deployment)
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
                Ok(Value::Array(keys.into_iter().map(|k| json!(k)).collect()))
            }
            _ => err("objectKeys: an object was expected."),
        }),
        "shallowmerge" => need(name, v, 1).and_then(|_| match &v[0] {
            Value::Array(a) => {
                let mut out = Map::new();
                for x in a {
                    let Value::Object(o) = x else { return err("shallowMerge: every element of the array must be an object.") };
                    for (k, y) in o {
                        out.insert(k.clone(), y.clone());
                    }
                }
                Ok(Value::Object(out))
            }
            _ => err("shallowMerge: an array of objects was expected."),
        }),
        // ---- template-functions-string
        "join" => need(name, v, 2).and_then(|_| match &v[0] {
            Value::Array(a) => {
                let d = s(&v[1]);
                let total: u64 = a.iter().map(|x| s(x).len() as u64 + d.len() as u64).sum();
                if total > limits::MAX_VALUE_BYTES {
                    return invalid(format!("join: the result is larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
                }
                Ok(Value::String(a.iter().map(s).collect::<Vec<_>>().join(&d)))
            }
            _ => err("join: an array was expected."),
        }),
        "like" => need(name, v, 2).map(|_| json!(like(&s(&v[0]).to_lowercase(), &s(&v[1]).to_lowercase()))),
        "base64tojson" => need(name, v, 1).and_then(|_| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s(&v[0]))
                .map_err(|_| EvalError::Msg("base64ToJson: the value is not valid base64.".into()))?;
            serde_json::from_slice(&bytes).map_err(|e| EvalError::Msg(format!("base64ToJson: the decoded text is not JSON: {e}")))
        }),
        // data URI as in the page's example: `data:text/plain;charset=utf-8;base64,SGVsbG8=` for 'Hello'
        "datauri" => need(name, v, 1).map(|_| {
            Value::String(format!("data:text/plain;charset=utf-8;base64,{}", base64::engine::general_purpose::STANDARD.encode(s(&v[0]))))
        }),
        "datauritostring" => need(name, v, 1).and_then(|_| data_uri_to_string(&s(&v[0])).map(Value::String)),
        "uri" => need(name, v, 2).and_then(|_| resolve_uri(&s(&v[0]), &s(&v[1])).map(Value::String)),
        "uricomponent" => need(name, v, 1).map(|_| Value::String(escape_data_string(&s(&v[0])))),
        "uricomponenttostring" => need(name, v, 1).map(|_| Value::String(unescape(&s(&v[0])))),
        _ => return None,
    })
}

/// `like`: `*` stands for any run of characters; both sides are already lower case.
///
/// The pattern is cut at its stars: the first piece has to start the text, the last one end it, and the pieces between them are
/// looked for one after the other, each at its first place after the one before (which leaves the most text for the rest). The
/// search in a `str` takes time in proportion to the text, so the whole takes time in proportion to the text and the pattern; a
/// matcher that goes back to the last star on a miss takes the product of the two.
fn like(text: &str, pattern: &str) -> bool {
    let mut pieces = pattern.split('*');
    let first = pieces.next().unwrap_or("");
    let Some(mut rest) = text.strip_prefix(first) else { return false };
    let pieces: Vec<&str> = pieces.collect();
    let Some((last, middle)) = pieces.split_last() else {
        // no star: the whole text is the pattern
        return rest.is_empty();
    };
    for piece in middle.iter().filter(|p| !p.is_empty()) {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// The index, in characters of `text`, of the character that the byte `lowered_at` of the lower-cased text belongs to. A letter
/// can be longer when it is lower-cased (`İ` is two characters), so a position in the lower-cased text is not one in `text`.
fn char_at_lowered_byte(text: &str, lowered_at: usize) -> i64 {
    let mut seen = 0usize;
    for (i, c) in text.chars().enumerate() {
        let width: usize = c.to_lowercase().map(char::len_utf8).sum();
        if seen + width > lowered_at {
            return i as i64;
        }
        seen += width;
    }
    text.chars().count() as i64
}

/// `Uri.EscapeDataString`: everything but the unreserved characters is percent-encoded (upper case hex).
fn escape_data_string(t: &str) -> String {
    let mut out = String::new();
    for b in t.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape(t: &str) -> String {
    let b = t.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn data_uri_to_string(t: &str) -> Result<String, EvalError> {
    let rest = t.strip_prefix("data:").ok_or_else(|| EvalError::Msg("dataUriToString: the value does not start with 'data:'.".into()))?;
    let (meta, data) = rest.split_once(',').ok_or_else(|| EvalError::Msg("dataUriToString: the data URI has no ','.".into()))?;
    if meta.split(';').any(|m| m.eq_ignore_ascii_case("base64")) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| EvalError::Msg("dataUriToString: the data is not valid base64.".into()))?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    } else {
        Ok(unescape(data))
    }
}

/// RFC 3986 section 5 (what `new Uri(base, relative)` does), for a base with a scheme.
fn resolve_uri(base: &str, rel: &str) -> Result<String, EvalError> {
    fn split(u: &str) -> (Option<&str>, Option<&str>, &str, Option<&str>, Option<&str>) {
        let (rest, frag) = match u.split_once('#') {
            Some((a, b)) => (a, Some(b)),
            None => (u, None),
        };
        let (rest, query) = match rest.split_once('?') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        let scheme_end = rest.find(':').filter(|i| *i > 0 && rest[..*i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
        let (scheme, rest) = match scheme_end {
            Some(i) => (Some(&rest[..i]), &rest[i + 1..]),
            None => (None, rest),
        };
        let (authority, path) = match rest.strip_prefix("//") {
            Some(r) => {
                let end = r.find('/').unwrap_or(r.len());
                (Some(&r[..end]), &r[end..])
            }
            None => (None, rest),
        };
        (scheme, authority, path, query, frag)
    }
    fn remove_dots(path: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        let segs: Vec<&str> = path.split('/').collect();
        for (i, seg) in segs.iter().enumerate() {
            match *seg {
                "." => {
                    if i == segs.len() - 1 {
                        out.push("");
                    }
                }
                ".." => {
                    if out.len() > 1 {
                        out.pop();
                    }
                    if i == segs.len() - 1 {
                        out.push("");
                    }
                }
                other => out.push(other),
            }
        }
        out.join("/")
    }
    let (bs, ba, bp, bq, _) = split(base);
    let bs = bs.ok_or_else(|| EvalError::Msg(format!("uri: '{base}' is not an absolute URI.")))?;
    let (rs, ra, rp, rq, rf) = split(rel);
    let (scheme, authority, path, query);
    if let Some(rs) = rs {
        (scheme, authority, path, query) = (rs, ra, remove_dots(rp), rq);
    } else if ra.is_some() {
        (scheme, authority, path, query) = (bs, ra, remove_dots(rp), rq);
    } else if rp.is_empty() {
        (scheme, authority, path, query) = (bs, ba, bp.to_string(), if rq.is_some() { rq } else { bq });
    } else if rp.starts_with('/') {
        (scheme, authority, path, query) = (bs, ba, remove_dots(rp), rq);
    } else {
        let merged = if ba.is_some() && bp.is_empty() {
            format!("/{rp}")
        } else {
            match bp.rfind('/') {
                Some(i) => format!("{}{rp}", &bp[..=i]),
                None => rp.to_string(),
            }
        };
        (scheme, authority, path, query) = (bs, ba, remove_dots(&merged), rq);
    }
    let mut out = format!("{scheme}:");
    if let Some(a) = authority {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(&path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = rf {
        out.push('#');
        out.push_str(f);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_wildcards() {
        assert!(like("prod-eastus-vm01", "*prod*"));
        assert!(like("prod-eastus-vm01", "prod*"));
        assert!(like("prod-eastus-vm01", "*01"));
        assert!(like("prod-eastus-vm01", "prod-eastus-vm01"));
        assert!(!like("prod-eastus-vm01", "*westus*"));
        assert!(like("", "*"));
        assert!(!like("abc", "ab"));
        assert!(like("abcabc", "a*c"));
        assert!(like("a", "a**"));
    }

    #[test]
    fn uri_follows_rfc_3986() {
        // the five examples of the page "template-functions-string#uri"
        let u = |b: &str, r: &str| resolve_uri(b, r).ok().unwrap();
        assert_eq!(u("http://contoso.org/firstpath", "myscript.sh"), "http://contoso.org/myscript.sh");
        assert_eq!(u("http://contoso.org/firstpath/", "myscript.sh"), "http://contoso.org/firstpath/myscript.sh");
        assert_eq!(u("http://contoso.org/firstpath/", "/myscript.sh"), "http://contoso.org/myscript.sh");
        assert_eq!(u("http://contoso.org/firstpath/azuredeploy.json", "myscript.sh"), "http://contoso.org/firstpath/myscript.sh");
        assert_eq!(
            u("http://contoso.org/firstpath/azuredeploy.json/", "myscript.sh"),
            "http://contoso.org/firstpath/azuredeploy.json/myscript.sh"
        );
        assert_eq!(u("http://contoso.com/resources/", "nested/azuredeploy.json"), "http://contoso.com/resources/nested/azuredeploy.json");
        assert_eq!(u("http://a/b/c/d;p?q", "../g"), "http://a/b/g");
        assert_eq!(u("http://a/b/c/d;p?q", "?y"), "http://a/b/c/d;p?y");
        assert_eq!(u("http://a/b/c/d;p?q", "//g"), "http://g");
        assert!(resolve_uri("relative/path", "x").is_err());
    }

    #[test]
    fn uri_component_round_trip() {
        let e = escape_data_string("http://contoso.com/resources/nested/azuredeploy.json");
        assert_eq!(e, "http%3A%2F%2Fcontoso.com%2Fresources%2Fnested%2Fazuredeploy.json");
        assert_eq!(unescape(&e), "http://contoso.com/resources/nested/azuredeploy.json");
        assert_eq!(escape_data_string("a b~é"), "a%20b~%C3%A9");
        assert_eq!(unescape("100%"), "100%");
        assert_eq!(unescape("%zz%41"), "%zzA");
    }

    #[test]
    fn data_uris() {
        assert_eq!(data_uri_to_string("data:;base64,SGVsbG8sIFdvcmxkIQ==").ok().unwrap(), "Hello, World!");
        assert_eq!(data_uri_to_string("data:text/plain,a%20b").ok().unwrap(), "a b");
        assert!(data_uri_to_string("nope").is_err());
    }

    /// The plain matcher that `like` used before it was made linear: the answers must be the same.
    fn like_reference(text: &str, pattern: &str) -> bool {
        let t: Vec<char> = text.chars().collect();
        let p: Vec<char> = pattern.chars().collect();
        let (mut i, mut j) = (0usize, 0usize);
        let (mut star, mut mark) = (None, 0usize);
        while i < t.len() {
            if j < p.len() && p[j] != '*' && p[j] == t[i] {
                i += 1;
                j += 1;
            } else if j < p.len() && p[j] == '*' {
                star = Some(j);
                mark = i;
                j += 1;
            } else if let Some(sj) = star {
                j = sj + 1;
                mark += 1;
                i = mark;
            } else {
                return false;
            }
        }
        while j < p.len() && p[j] == '*' {
            j += 1;
        }
        j == p.len()
    }

    #[test]
    fn like_gives_the_answers_of_the_plain_matcher() {
        // every text and pattern of up to 7 characters over a small alphabet that has the wildcard
        let alphabet = ['a', 'b', '*', 'é'];
        let mut words = vec![String::new()];
        let mut layer = vec![String::new()];
        for _ in 0..6 {
            layer = layer.iter().flat_map(|w| alphabet.iter().map(move |c| format!("{w}{c}"))).collect();
            words.extend(layer.iter().cloned());
        }
        let texts: Vec<&String> = words.iter().filter(|w| !w.contains('*')).collect();
        let mut n = 0u64;
        for (i, pattern) in words.iter().enumerate() {
            // all the patterns against a spread of texts (all of them would be 5,000 x 1,400,000)
            for text in texts.iter().skip(i % 7).step_by(97) {
                assert_eq!(like(text, pattern), like_reference(text, pattern), "text '{text}' pattern '{pattern}'");
                n += 1;
            }
        }
        assert!(n > 50_000, "{n}");
    }

    #[test]
    fn like_takes_linear_time() {
        // 40,000 characters against 20,000: the plain matcher needed minutes for the last of these
        let text = "a".repeat(40_000);
        let t = std::time::Instant::now();
        assert!(!like(&text, &format!("{}b", "*a".repeat(10_000))));
        assert!(!like(&text, &format!("{}*b*", "a*".repeat(10_000))));
        assert!(like(&text, &format!("{}a", "*a".repeat(10_000))));
        assert!(!like(&format!("{text}c"), &format!("{}*b", "a*".repeat(10_000))));
        assert!(t.elapsed() < std::time::Duration::from_secs(3), "took {:?}", t.elapsed());
    }

    fn call_ok(name: &str, args: &[Value]) -> Value {
        match call(name, &name.to_lowercase(), args) {
            Some(Ok(v)) => v,
            Some(Err(e)) => panic!("{name}: {}", e.text()),
            None => panic!("{name}: not a function of this file"),
        }
    }

    #[test]
    fn index_of_counts_the_characters_of_the_text_it_was_given() {
        use serde_json::json;
        // 'İ' (U+0130) is two characters (and three bytes) when lower-cased: the answer is a position in the text given
        assert_eq!(call_ok("indexOf", &[json!("İİx"), json!("x")]), json!(2));
        assert_eq!(call_ok("lastIndexOf", &[json!("İİxx"), json!("x")]), json!(3));
        assert_eq!(call_ok("indexOf", &[json!("İİx"), json!("İx")]), json!(1));
        assert_eq!(call_ok("indexOf", &[json!("ÄÖx"), json!("ö")]), json!(1));
        assert_eq!(call_ok("indexOf", &[json!("Hello World"), json!("WORLD")]), json!(6));
        assert_eq!(call_ok("indexOf", &[json!("abc"), json!("")]), json!(0));
        assert_eq!(call_ok("indexOf", &[json!("abc"), json!("z")]), json!(-1));
        assert_eq!(call_ok("lastIndexOf", &[json!("abcabc"), json!("B")]), json!(4));
        assert_eq!(call_ok("lastIndexOf", &[json!("abc"), json!("")]), json!(3));
    }
}
