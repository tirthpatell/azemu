//! Avro schemas for the Schema Registry: what is refused on registration, the compact form that is stored, and the
//! compatibility check of a group's mode (Forward, Backward).
//!
//! **What Azure does** (probed against a Standard namespace in November 2026, `docs/services/schema-registry.md`):
//! the content has to be a JSON object; a record without `name` or `fields`, a field without `type`, an enum without
//! symbols, a `fixed` without `size`, an `array` without `items` and a duplicate field name are refused with the texts
//! below. Azure accepts what the Avro library for .NET accepts: a record name such as `1bad`, a `default` that does not
//! fit the type and a `type` that names nothing (`{"type":"foo"}`) all pass, and so they do here. The content is stored
//! as compact JSON in the order it came in, and two contents that differ only in white space or in the order of the
//! keys of an object are one schema.
//!
//! The compatibility check is **not** Azure's code. It is Avro's rule (can the reader read what the writer wrote?) with
//! the differences found by trying about sixty pairs of schemas: see [`compatible`].

use std::collections::{HashMap, HashSet};

use serde_json::Value;

const PRIMITIVES: &[&str] = &["null", "boolean", "int", "long", "float", "double", "bytes", "string"];

/// A schema that passed [`parse`].
pub struct Parsed {
    /// What is stored and handed back: the JSON without white space.
    pub compact: String,
    /// For comparing: equal when the schemas are the same schema.
    pub value: Value,
}

/// The text of the refusal (without the `Avro schema validation failed: ` Azure puts in front of it).
pub fn parse(text: &str) -> Result<Parsed, String> {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        // serde_json counts the levels while it reads, so a deep schema never reaches the recursive code below
        Err(e) if e.to_string().starts_with("recursion limit exceeded") => return Err(too_deep()),
        // Newtonsoft reads the first token before it notices what follows: `[` alone is "not an object", not "unexpected end"
        Err(_) if text.trim_start().starts_with('[') => return Err(not_an_object(&Value::Array(vec![]), text)),
        Err(e) => return Err(json_error_text(text, &e)),
    };
    if !value.is_object() {
        return Err(not_an_object(&value, text));
    }
    check(&value, "")?;
    let compact = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    Ok(Parsed { compact, value })
}

/// What serde_json allows: 128 levels of `[` and `{`.
const MAX_JSON_DEPTH: usize = 128;

fn too_deep() -> String {
    format!("The schema is nested more than {MAX_JSON_DEPTH} levels deep (a limit of the emulator's JSON reader, not of Azure).")
}

fn json_error_text(text: &str, e: &serde_json::Error) -> String {
    use serde_json::error::Category;
    match e.classify() {
        Category::Eof => {
            format!("Unexpected end of content while loading JObject. Path '', line {}, position {}.", e.line().max(1), e.column())
        }
        _ => {
            // Newtonsoft counts a first line from 0 and names the character it could not read
            let ch = text.lines().nth(e.line().saturating_sub(1)).and_then(|l| l.chars().nth(e.column().saturating_sub(1))).unwrap_or(' ');
            format!(
                "Unexpected character encountered while parsing value: {ch}. Path '', line {}, position {}.",
                e.line().saturating_sub(1),
                e.column().saturating_sub(1)
            )
        }
    }
}

fn not_an_object(v: &Value, text: &str) -> String {
    let (token, position) = match v {
        Value::Array(_) => ("StartArray", 1),
        Value::String(_) => ("String", text.trim().chars().count()),
        Value::Number(n) => (if n.is_f64() { "Float" } else { "Integer" }, text.trim().chars().count()),
        Value::Bool(_) => ("Boolean", text.trim().chars().count()),
        _ => ("Null", text.trim().chars().count()),
    };
    format!(
        "Error reading JObject from JsonReader. Current JsonReader item is not an object: {token}. Path '', line 1, position {position}."
    )
}

/// `{  "type": "fixed",  "name": "F"}`: how the Avro library for .NET prints a JSON object into a message.
fn flat(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default().replace('\n', "")
}

fn check(v: &Value, path: &str) -> Result<(), String> {
    match v {
        Value::Array(branches) => branches.iter().try_for_each(|b| check(b, path)),
        Value::Object(o) => {
            let Some(t) = o.get("type") else { return Err(format!("Property type is required at '{path}'.")) };
            match t {
                Value::String(kind) => match kind.as_str() {
                    "record" | "error" => check_record(v, path),
                    "enum" => {
                        let ok = o.get("symbols").and_then(Value::as_array).is_some_and(|s| !s.is_empty());
                        if ok {
                            Ok(())
                        } else {
                            Err(format!("Enum has no symbols: {} at '{path}'.", o.get("name").and_then(Value::as_str).unwrap_or("")))
                        }
                    }
                    "fixed" => {
                        if o.get("size").is_some() {
                            Ok(())
                        } else {
                            Err(format!("No \"size\" JSON field: {} at '{path}' at '{path}'.", flat(v)))
                        }
                    }
                    "array" => match o.get("items") {
                        Some(items) => check(items, path),
                        None => Err(format!("Array does not have 'items' at '{path}'.")),
                    },
                    "map" => match o.get("values") {
                        Some(values) => check(values, path),
                        None => Err(format!("Map does not have 'values' at '{path}'.")),
                    },
                    _ => Ok(()),
                },
                other => check(other, path),
            }
        }
        _ => Ok(()),
    }
}

fn check_record(v: &Value, path: &str) -> Result<(), String> {
    let o = v.as_object().ok_or("record is not an object")?;
    if o.get("name").and_then(Value::as_str).is_none() {
        return Err(format!("name cannot be null for record schema. at '{path}'."));
    }
    let Some(fields) = o.get("fields").and_then(Value::as_array) else {
        return Err(format!("'fields' cannot be null for record at '{path}'."));
    };
    let mut seen = HashSet::new();
    for (i, f) in fields.iter().enumerate() {
        let here = if path.is_empty() { format!("fields[{i}]") } else { format!("{path}.fields[{i}]") };
        let Some(fo) = f.as_object() else { return Err(format!("field is not an object at '{here}'.")) };
        let name = fo.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(ft) = fo.get("type") else { return Err(format!("'type' was not found for field: name at '{here}'.")) };
        if !seen.insert(name.to_string()) {
            return Err(format!("field or alias {name} is a duplicate name at '{here}'."));
        }
        check(ft, &here)?;
    }
    Ok(())
}

// ---- compatibility --------------------------------------------------------------------------------------------

/// How many levels of types the comparison follows (a record in a field in a record counts two; a name that leads to a record
/// counts like the record written in place). A count, not a guess about the size of the stack. The JSON reader stops at 128
/// levels of brackets, so only names can get deeper than this.
const MAX_DEPTH: usize = 64;

/// How many pairs of types one comparison may look at. Real schemas need a few hundred; this is the emulator's own limit.
const MAX_STEPS: usize = 200_000;

/// What a schema (or a name that stands for one) is, with the namespace in force where it was written. It borrows from the two
/// schemas being compared, never from the tables of names, so a comparison may go on while the tables are in use.
#[derive(Clone)]
enum Kind<'a> {
    Prim(&'a str, Option<&'a str>),
    Record {
        full: String,
        ns: String,
        fields: &'a [Value],
        /// the object itself: two kinds with the same node are the same type (what the remembered answers are keyed by)
        node: &'a Value,
    },
    Enum {
        full: String,
    },
    Fixed {
        full: String,
        size: Option<&'a Value>,
    },
    Array(&'a Value, String),
    Map(&'a Value, String),
    Union(&'a [Value], String),
    /// a name that was not defined anywhere in the schema: only equal to the same name
    Named(String),
}

struct Names<'a> {
    defined: HashMap<String, (&'a Value, String)>,
}

fn full_name(o: &serde_json::Map<String, Value>, enclosing: &str) -> (String, String) {
    let name = o.get("name").and_then(Value::as_str).unwrap_or("");
    if let Some((ns, _)) = name.rsplit_once('.') {
        return (name.to_string(), ns.to_string());
    }
    let ns = o.get("namespace").and_then(Value::as_str).unwrap_or(enclosing);
    if ns.is_empty() {
        (name.to_string(), String::new())
    } else {
        (format!("{ns}.{name}"), ns.to_string())
    }
}

impl<'a> Names<'a> {
    fn of(root: &'a Value) -> Names<'a> {
        let mut n = Names { defined: HashMap::new() };
        n.collect(root, "");
        n
    }

    /// The depth of `v` is at most what the JSON reader allows (128 levels), so this recursion is bounded.
    fn collect(&mut self, v: &'a Value, enclosing: &str) {
        match v {
            Value::Array(a) => a.iter().for_each(|b| self.collect(b, enclosing)),
            Value::Object(o) => {
                let kind = o.get("type").and_then(Value::as_str);
                match kind {
                    Some("record") | Some("error") => {
                        let (full, ns) = full_name(o, enclosing);
                        self.defined.insert(full, (v, enclosing.to_string()));
                        for f in o.get("fields").and_then(Value::as_array).into_iter().flatten() {
                            if let Some(t) = f.get("type") {
                                self.collect(t, &ns);
                            }
                        }
                    }
                    Some("enum") | Some("fixed") => {
                        let (full, _) = full_name(o, enclosing);
                        self.defined.insert(full, (v, enclosing.to_string()));
                    }
                    Some("array") => {
                        if let Some(i) = o.get("items") {
                            self.collect(i, enclosing);
                        }
                    }
                    Some("map") => {
                        if let Some(i) = o.get("values") {
                            self.collect(i, enclosing);
                        }
                    }
                    _ => {
                        if let Some(t) = o.get("type") {
                            self.collect(t, enclosing);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Names lead only to records, enums and fixed types (the table holds nothing else), which are answered without looking
    /// further, so this recursion is bounded by the depth of `v` itself.
    fn kind(&self, v: &'a Value, enclosing: &str) -> Kind<'a> {
        match v {
            Value::String(s) => {
                if let Some(p) = PRIMITIVES.iter().find(|p| **p == s) {
                    return Kind::Prim(p, None);
                }
                let tries =
                    if s.contains('.') || enclosing.is_empty() { vec![s.clone()] } else { vec![format!("{enclosing}.{s}"), s.clone()] };
                for t in tries {
                    if let Some((def, def_enclosing)) = self.defined.get(&t) {
                        return self.kind(def, def_enclosing);
                    }
                }
                Kind::Named(s.clone())
            }
            Value::Array(a) => Kind::Union(a, enclosing.to_string()),
            Value::Object(o) => match o.get("type") {
                Some(tv @ Value::String(t)) => match t.as_str() {
                    "record" | "error" => {
                        let (full, ns) = full_name(o, enclosing);
                        Kind::Record {
                            full,
                            ns,
                            fields: o.get("fields").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]),
                            node: v,
                        }
                    }
                    "enum" => Kind::Enum { full: full_name(o, enclosing).0 },
                    "fixed" => Kind::Fixed { full: full_name(o, enclosing).0, size: o.get("size") },
                    "array" => Kind::Array(o.get("items").unwrap_or(&Value::Null), enclosing.to_string()),
                    "map" => Kind::Map(o.get("values").unwrap_or(&Value::Null), enclosing.to_string()),
                    prim => match PRIMITIVES.iter().find(|p| **p == prim) {
                        Some(p) => Kind::Prim(p, o.get("logicalType").and_then(Value::as_str)),
                        None => self.kind(tv, enclosing),
                    },
                },
                Some(other) => self.kind(other, enclosing),
                None => Kind::Named(String::new()),
            },
            _ => Kind::Named(String::new()),
        }
    }
}

struct Check<'a> {
    reader: Names<'a>,
    writer: Names<'a>,
    /// pairs of named types being compared: a type that contains itself is compatible with itself
    open: HashSet<(String, String)>,
    /// what was found for a pair of record nodes (and whether it was a field of the top record): the same pair is met again
    /// and again when many fields have the same type, and without this the time is exponential in the depth
    done: HashMap<(usize, usize, bool), bool>,
    /// how many times a pair in `open` was taken to be compatible with itself: a result that rests on none of them is final
    assumed: usize,
    depth: usize,
    steps: usize,
    max_steps: usize,
    /// set when a limit was hit: from then on every comparison answers false at once, and the verdict is the message
    stopped: Option<String>,
}

fn promotable(writer: &str, reader: &str) -> bool {
    matches!(
        (writer, reader),
        ("int", "long")
            | ("int", "float")
            | ("int", "double")
            | ("long", "float")
            | ("long", "double")
            | ("float", "double")
            | ("string", "bytes")
            | ("bytes", "string")
    )
}

/// Can a reader that has the schema `reader` read what a writer with the schema `writer` wrote?
///
/// This is Avro's schema resolution, except for what a Standard namespace was seen to do differently (each of these is a
/// pair of schemas that Azure refused or accepted; `docs/services/schema-registry.md`):
///
/// * the type of a field of the top record has to be the same: a number that Avro would widen (`int` to `long`,
///   `float` to `double`) is refused there, and so is a union on one side and a plain type on the other, and a union
///   branch that is not the same type; deeper down (a nested record, an array, a map, a top-level number) Avro's widening
///   is applied;
/// * the enum symbols are not compared, only the enum's name; aliases are ignored (a renamed record or field is
///   refused); a `logicalType` has to be the same.
///
/// `Err` is the text of the limit that stopped the comparison (the emulator's own, not Azure's): the schemas nest more than
/// [`MAX_DEPTH`] levels, or the comparison needed more than [`MAX_STEPS`] steps.
pub fn compatible(reader: &Value, writer: &Value) -> Result<bool, String> {
    compatible_within(reader, writer, MAX_STEPS)
}

fn compatible_within(reader: &Value, writer: &Value, max_steps: usize) -> Result<bool, String> {
    let mut c = Check {
        reader: Names::of(reader),
        writer: Names::of(writer),
        open: HashSet::new(),
        done: HashMap::new(),
        assumed: 0,
        depth: 0,
        steps: 0,
        max_steps,
        stopped: None,
    };
    let ok = c.can_read(reader, "", writer, "", false, true);
    match c.stopped {
        Some(message) => Err(message),
        None => Ok(ok),
    }
}

impl<'a> Check<'a> {
    /// `strict`: the types of the fields of the top record (see [`compatible`]); `top`: this is the schema itself.
    fn can_read(&mut self, r: &'a Value, rns: &str, w: &'a Value, wns: &str, strict: bool, top: bool) -> bool {
        let rk = self.reader.kind(r, rns);
        let wk = self.writer.kind(w, wns);
        self.kinds(&rk, &wk, strict, top)
    }

    /// Counts one step against the limits; `false` when a limit was hit (now or before) and the comparison is to end.
    fn tick(&mut self) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        self.steps += 1;
        if self.steps > self.max_steps {
            self.stopped = Some(format!(
                "the compatibility check was stopped after {} steps: the schemas are too complex to compare (a limit of the emulator, not of Azure).",
                self.max_steps
            ));
            return false;
        }
        if self.depth >= MAX_DEPTH {
            self.stopped = Some(format!(
                "the compatibility check was stopped: the schemas nest more than {MAX_DEPTH} levels, counting the types that a name leads to (a limit of the emulator, not of Azure)."
            ));
            return false;
        }
        true
    }

    /// One step of the comparison, counted against the limits.
    fn kinds(&mut self, r: &Kind<'a>, w: &Kind<'a>, root: bool, top: bool) -> bool {
        if !self.tick() {
            return false;
        }
        self.depth += 1;
        let ok = self.compare(r, w, root, top);
        self.depth -= 1;
        ok
    }

    fn compare(&mut self, r: &Kind<'a>, w: &Kind<'a>, root: bool, top: bool) -> bool {
        // a union on the writer's side: every branch must be readable
        if let Kind::Union(wb, wns) = w {
            return wb.iter().all(|b| match r {
                Kind::Union(rb, rns) => rb.iter().any(|rbv| self.same_or_readable(rbv, rns, b, wns, root)),
                _ => !root && self.can_read_kind(r, b, wns),
            });
        }
        if let Kind::Union(rb, rns) = r {
            return !root && rb.iter().any(|rbv| self.can_read_kind_w(rbv, rns, w));
        }
        match (r, w) {
            (Kind::Prim(rn, rl), Kind::Prim(wn, wl)) => {
                if rl != wl {
                    return false;
                }
                rn == wn || (!root && promotable(wn, rn))
            }
            (
                Kind::Record { full: rf, ns: rns, fields: rfields, node: rnode },
                Kind::Record { full: wf, ns: wns, fields: wfields, node: wnode },
            ) => {
                if rf != wf {
                    return false;
                }
                let memo = (std::ptr::from_ref(*rnode) as usize, std::ptr::from_ref(*wnode) as usize, top);
                if let Some(known) = self.done.get(&memo) {
                    return *known;
                }
                let pair = (rf.clone(), wf.clone());
                if !self.open.insert(pair.clone()) {
                    self.assumed += 1;
                    return true;
                }
                let assumed_before = self.assumed;
                let ok = rfields.iter().all(|rfield| {
                    let name = rfield.get("name").and_then(Value::as_str).unwrap_or("");
                    match wfields.iter().find(|f| f.get("name").and_then(Value::as_str) == Some(name)) {
                        Some(wfield) => {
                            let (rt, wt) = (rfield.get("type").unwrap_or(&Value::Null), wfield.get("type").unwrap_or(&Value::Null));
                            self.can_read(rt, rns, wt, wns, top, false)
                        }
                        None => rfield.get("default").is_some(),
                    }
                });
                self.open.remove(&pair);
                // an answer of "no" holds whatever was assumed on the way (the assumptions only made it easier to say yes); an
                // answer of "yes" holds only when it did not rest on a type that was still being compared
                if self.stopped.is_none() && (!ok || self.assumed == assumed_before) {
                    self.done.insert(memo, ok);
                }
                ok
            }
            (Kind::Enum { full: rf }, Kind::Enum { full: wf }) => rf == wf,
            (Kind::Fixed { full: rf, size: rs }, Kind::Fixed { full: wf, size: ws }) => rf == wf && rs == ws,
            (Kind::Array(ri, rns), Kind::Array(wi, wns)) => self.can_read(ri, rns, wi, wns, false, false),
            (Kind::Map(ri, rns), Kind::Map(wi, wns)) => self.can_read(ri, rns, wi, wns, false, false),
            (Kind::Named(a), Kind::Named(b)) => a == b,
            _ => false,
        }
    }

    /// Two branches of unions on both sides, for the strict top-record fields: the same type, and readable.
    fn same_or_readable(&mut self, r: &'a Value, rns: &str, w: &'a Value, wns: &str, root: bool) -> bool {
        if !self.tick() {
            return false;
        }
        let rk = self.reader.kind(r, rns);
        let wk = self.writer.kind(w, wns);
        if root && !same_type(&rk, &wk) {
            return false;
        }
        self.kinds(&rk, &wk, false, false)
    }

    fn can_read_kind(&mut self, r: &Kind<'a>, w: &'a Value, wns: &str) -> bool {
        let wk = self.writer.kind(w, wns);
        self.kinds(r, &wk, false, false)
    }

    fn can_read_kind_w(&mut self, r: &'a Value, rns: &str, w: &Kind<'a>) -> bool {
        let rk = self.reader.kind(r, rns);
        self.kinds(&rk, w, false, false)
    }
}

fn same_type(a: &Kind, b: &Kind) -> bool {
    match (a, b) {
        (Kind::Prim(x, _), Kind::Prim(y, _)) => x == y,
        (Kind::Record { full: x, .. }, Kind::Record { full: y, .. }) => x == y,
        (Kind::Enum { full: x }, Kind::Enum { full: y }) => x == y,
        (Kind::Fixed { full: x, .. }, Kind::Fixed { full: y, .. }) => x == y,
        (Kind::Array(..), Kind::Array(..)) | (Kind::Map(..), Kind::Map(..)) => true,
        (Kind::Named(x), Kind::Named(y)) => x == y,
        _ => false,
    }
}

/// A number for a schema that is the same for two schemas that are equal as values (the order of the keys of an object does not
/// count). Used to find the version that has some content without reading every stored version again. Only meaningful within
/// one run of the process.
pub fn canonical_hash(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    fn feed(v: &Value, h: &mut std::collections::hash_map::DefaultHasher) {
        match v {
            Value::Null => 0u8.hash(h),
            Value::Bool(b) => (1u8, *b).hash(h),
            Value::Number(n) => (2u8, n.to_string()).hash(h),
            Value::String(s) => (3u8, s).hash(h),
            Value::Array(a) => {
                (4u8, a.len()).hash(h);
                a.iter().for_each(|x| feed(x, h));
            }
            Value::Object(o) => {
                (5u8, o.len()).hash(h);
                let mut entries: Vec<(&String, &Value)> = o.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (k, x) in entries {
                    k.hash(h);
                    feed(x, h);
                }
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    feed(v, &mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Value {
        parse(s).unwrap().value
    }

    fn rec(fields: &str) -> String {
        format!(r#"{{"type":"record","name":"R","fields":[{fields}]}}"#)
    }

    /// Forward: the old schema reads what the new one writes.
    fn forward(old: &str, new: &str) -> bool {
        compatible(&p(old), &p(new)).unwrap()
    }

    #[test]
    fn compact_form_and_equality() {
        let a = parse("{ \"type\" : \"record\",\n \"name\":\"S\", \"fields\":[ {\"name\":\"x\",\"type\":\"int\"} ] }").unwrap();
        assert_eq!(a.compact, r#"{"type":"record","name":"S","fields":[{"name":"x","type":"int"}]}"#);
        let b = parse(r#"{"name":"S","type":"record","fields":[{"type":"int","name":"x"}]}"#).unwrap();
        assert_eq!(a.value, b.value, "the order of keys does not matter");
    }

    #[test]
    fn refusals_have_azures_texts() {
        let e = |s: &str| parse(s).err().unwrap_or_default();
        assert_eq!(e("{}"), "Property type is required at ''.");
        assert_eq!(e(r#"{"type":"record"}"#), "name cannot be null for record schema. at ''.");
        assert_eq!(e(r#"{"type":"record","name":"X"}"#), "'fields' cannot be null for record at ''.");
        assert_eq!(e(r#"{"type":"record","name":"X","fields":[{"name":"a"}]}"#), "'type' was not found for field: name at 'fields[0]'.");
        assert_eq!(
            e(r#"{"type":"record","name":"X","fields":[{"name":"a","type":"int"},{"name":"a","type":"int"}]}"#),
            "field or alias a is a duplicate name at 'fields[1]'."
        );
        assert_eq!(e(r#"{"type":"enum","name":"E"}"#), "Enum has no symbols: E at ''.");
        assert_eq!(e(r#"{"type":"fixed","name":"F"}"#), "No \"size\" JSON field: {  \"type\": \"fixed\",  \"name\": \"F\"} at '' at ''.");
        assert_eq!(e(r#"{"type":"array"}"#), "Array does not have 'items' at ''.");
        assert!(e("[").starts_with("Error reading JObject from JsonReader. Current JsonReader item is not an object: StartArray."));
        assert!(e("hello").starts_with("Unexpected character encountered while parsing value: h."));
        assert!(e(r#"{"type":"record""#).starts_with("Unexpected end of content"));
    }

    #[test]
    fn what_azure_accepts_is_accepted() {
        for ok in [
            r#"{"type":"foo"}"#,
            r#"{"type":"record","name":"1bad","fields":[]}"#,
            r#"{"type":"record","name":"X","fields":[{"name":"a","type":"int","default":"str"}]}"#,
            r#"{"type":"string"}"#,
        ] {
            assert!(parse(ok).is_ok(), "{ok}");
        }
    }

    // The pairs below are the ones tried against a Standard namespace with the group in Forward mode (old, new): the answer
    // in the comment is what Azure gave.
    #[test]
    fn forward_pairs_as_azure_answered() {
        let f = |n: &str, t: &str| format!(r#"{{"name":"{n}","type":{t}}}"#);
        let (a, b) = (f("a", r#""int""#), f("b", r#""int""#));
        // added field: accepted; field removed without default: refused; with default: accepted
        assert!(forward(&rec(&a), &rec(&format!("{a},{b}"))));
        assert!(!forward(&rec(&format!("{a},{b}")), &rec(&a)));
        assert!(forward(&rec(&format!(r#"{a},{{"name":"b","type":"int","default":1}}"#)), &rec(&a)));
        // numbers in a field of the top record: any change is refused, also the ones Avro allows
        assert!(!forward(&rec(&f("a", r#""int""#)), &rec(&f("a", r#""long""#))));
        assert!(!forward(&rec(&f("a", r#""long""#)), &rec(&f("a", r#""int""#))));
        assert!(!forward(&rec(&f("a", r#""double""#)), &rec(&f("a", r#""float""#))));
        assert!(!forward(&rec(&f("a", r#""string""#)), &rec(&f("a", r#""bytes""#))));
        // unions
        assert!(!forward(&rec(&f("a", r#"["null","string"]"#)), &rec(&f("a", r#""string""#))));
        assert!(!forward(&rec(&f("a", r#""string""#)), &rec(&f("a", r#"["null","string"]"#))));
        assert!(!forward(&rec(&f("a", r#"["null","string"]"#)), &rec(&f("a", r#"["null","string","int"]"#))));
        assert!(forward(&rec(&f("a", r#"["null","string","int"]"#)), &rec(&f("a", r#"["null","string"]"#))));
        assert!(forward(&rec(&f("a", r#"["null","string"]"#)), &rec(&f("a", r#"["string","null"]"#))));
        assert!(!forward(&rec(&f("a", r#"["null","long"]"#)), &rec(&f("a", r#"["null","int"]"#))));
        // names: a renamed record is refused, aliases do not help; the namespace is part of the name
        assert!(!forward(&rec(&a), r#"{"type":"record","name":"Other","fields":[{"name":"a","type":"int"}]}"#));
        assert!(!forward(&rec(&a), r#"{"type":"record","name":"Other","aliases":["R"],"fields":[{"name":"a","type":"int"}]}"#));
        assert!(!forward(
            r#"{"type":"record","name":"R","namespace":"x","fields":[]}"#,
            r#"{"type":"record","name":"R","namespace":"y","fields":[]}"#
        ));
        // a field renamed with an alias is refused
        assert!(!forward(&rec(&a), &rec(r#"{"name":"z","type":"int","aliases":["a"]}"#)));
        // enums: the name counts, the symbols do not
        let e = |n: &str, s: &str| f("a", &format!(r#"{{"type":"enum","name":"{n}","symbols":[{s}]}}"#));
        assert!(forward(&rec(&e("E", r#""A""#)), &rec(&e("E", r#""A","B""#))));
        assert!(forward(&rec(&e("E", r#""A""#)), &rec(&e("E", r#""Z""#))));
        assert!(!forward(&rec(&e("E", r#""A""#)), &rec(&e("E2", r#""A""#))));
        // fixed: name and size
        let x = |n: &str, s: u32| f("a", &format!(r#"{{"type":"fixed","name":"{n}","size":{s}}}"#));
        assert!(!forward(&rec(&x("F", 4)), &rec(&x("F", 8))));
        assert!(!forward(&rec(&x("F", 4)), &rec(&x("G", 4))));
        // deeper down Avro's widening is applied
        let n = |t: &str| f("a", &format!(r#"{{"type":"record","name":"N","fields":[{{"name":"x","type":"{t}"}}]}}"#));
        assert!(forward(&rec(&n("long")), &rec(&n("int"))));
        assert!(!forward(&rec(&n("int")), &rec(&n("string"))));
        assert!(forward(&rec(&f("a", r#"{"type":"array","items":"long"}"#)), &rec(&f("a", r#"{"type":"array","items":"int"}"#))));
        assert!(!forward(&rec(&f("a", r#"{"type":"array","items":"int"}"#)), &rec(&f("a", r#"{"type":"array","items":"long"}"#))));
        assert!(forward(&rec(&f("a", r#"{"type":"map","values":"long"}"#)), &rec(&f("a", r#"{"type":"map","values":"int"}"#))));
        assert!(forward(r#"{"type":"long"}"#, r#"{"type":"int"}"#));
        assert!(!forward(r#"{"type":"int"}"#, r#"{"type":"string"}"#));
        // a nested field removed is refused, one added is accepted
        let n2 = |fs: &str| f("a", &format!(r#"{{"type":"record","name":"N","fields":[{fs}]}}"#));
        assert!(!forward(&rec(&n2(r#"{"name":"x","type":"int"},{"name":"y","type":"int"}"#)), &rec(&n2(r#"{"name":"x","type":"int"}"#))));
        assert!(forward(&rec(&n2(r#"{"name":"x","type":"int"}"#)), &rec(&n2(r#"{"name":"x","type":"int"},{"name":"y","type":"int"}"#))));
        // logicalType
        assert!(!forward(&rec(&f("a", r#"{"type":"int","logicalType":"date"}"#)), &rec(&f("a", r#""int""#))));
        // a record that contains itself
        let list = r#"{"type":"record","name":"L","fields":[{"name":"v","type":"int"},{"name":"next","type":["null","L"]}]}"#;
        assert!(forward(list, list));
    }

    #[test]
    fn limits_end_the_comparison_with_their_own_text() {
        let list = r#"{"type":"record","name":"L","fields":[{"name":"v","type":"int"},{"name":"next","type":["null","L"]}]}"#;
        let l = p(list);
        // a budget that is too small for even this schema
        let e = compatible_within(&l, &l, 2).unwrap_err();
        assert!(e.contains("after 2 steps"), "{e}");
        assert_eq!(compatible_within(&l, &l, 1000), Ok(true));
        // names that lead 200 levels deep (the JSON itself is shallow)
        let mut fields = Vec::new();
        for i in 0..200 {
            let next = if i < 199 { format!("\"A{}\"", i + 1) } else { "\"int\"".into() };
            fields.push(format!(r#"{{"name":"d{i}","type":{{"type":"record","name":"A{i}","fields":[{{"name":"n","type":{next}}}]}}}}"#));
        }
        let deep = p(&format!(r#"{{"type":"record","name":"Root","fields":[{}]}}"#, fields.join(",")));
        let e = compatible(&deep, &deep).unwrap_err();
        assert!(e.contains("more than 64 levels"), "{e}");
    }

    #[test]
    fn remembered_answers_do_not_change_the_result() {
        // the same record type met many times: a "yes" and a "no" are both found again
        let t = |inner: &str| format!(r#"{{"type":"record","name":"T","fields":[{{"name":"x","type":{inner}}}]}}"#);
        let two = |ty: &str| rec(&format!(r#"{{"name":"a","type":{}}},{{"name":"b","type":"T"}},{{"name":"c","type":"T"}}"#, t(ty)));
        assert!(forward(&two(r#""long""#), &two(r#""int""#)));
        assert!(!forward(&two(r#""int""#), &two(r#""string""#)));
        // a record that holds itself through a union, twice, and one that is not the same on the second look
        let node = |extra: &str| {
            format!(
                r#"{{"type":"record","name":"N","fields":[{{"name":"l","type":["null","N"]}},{{"name":"r","type":["null","N"]}}{extra}]}}"#
            )
        };
        assert!(forward(&node(""), &node("")));
        assert!(!forward(&node(r#",{"name":"z","type":"int"}"#), &node("")));
    }

    #[test]
    fn json_nested_too_deep_is_refused_by_the_parser() {
        let e = parse(&format!("{}{}", "[".repeat(10_000), "]".repeat(10_000))).err().unwrap_or_default();
        assert!(e.contains("128 levels"), "{e}");
        let mut rec = r#""int""#.to_string();
        for i in 0..10_000 {
            rec = format!(r#"{{"type":"record","name":"R{i}","fields":[{{"name":"f","type":{rec}}}]}}"#);
        }
        assert!(parse(&rec).err().unwrap_or_default().contains("128 levels"));
    }
}
