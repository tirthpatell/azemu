//! Type checks for parameters, outputs and user-defined function parameters.
//!
//! A schema is what `parameters.{name}` holds in a template: `type`, `allowedValues`, `minValue`, `maxValue`,
//! `minLength`, `maxLength` and, in a `languageVersion` 2.0 template, `nullable`, `items`, `prefixItems`, `properties`,
//! `additionalProperties`, `discriminator` and `$ref` to an entry of `definitions` (Microsoft Learn,
//! `azure-resource-manager/templates/syntax#definitions` and `#parameters`; Bicep writes user-defined types this way).
//! A property that a `properties` schema names must be present unless its schema says `nullable`.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::limits::Budget;

/// How deep a value may be checked: the evaluator already refuses values nested more than 100 levels.
const MAX_DEPTH: usize = 110;

/// What checking one node of a value costs, besides the entries of the schema it has to look through.
const NODE_COST: usize = 16;

pub struct Types<'a> {
    pub definitions: &'a Map<String, Value>,
    /// The work of the request this check belongs to: each node of a value is charged to it, so that a value and a schema that
    /// are each small enough can not make a check that takes for ever. `None` for a check that has no request (the unit tests).
    pub budget: Option<&'a Budget>,
}

/// The JSON type names of the template language.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "bool",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

/// A schema with every `$ref` followed, without copying anything: the referenced definition with the keys of the referring
/// schema on top (`{"$ref": ..., "nullable": true}` is the definition made nullable). `layers` run from the outermost schema to
/// the last definition; a key is the one of the first layer that has it.
struct Resolved<'s> {
    layers: Vec<&'s Map<String, Value>>,
}

impl<'s> Resolved<'s> {
    fn get(&self, key: &str) -> Option<&'s Value> {
        self.layers.iter().find_map(|m| m.get(key))
    }
}

impl<'a> Types<'a> {
    pub fn new(definitions: &'a Map<String, Value>) -> Types<'a> {
        Types { definitions, budget: None }
    }

    pub fn with_budget(mut self, budget: &'a Budget) -> Types<'a> {
        self.budget = Some(budget);
        self
    }

    fn charge(&self, units: usize) -> Result<(), String> {
        match self.budget {
            Some(b) => b.charge(units as u64),
            None => Ok(()),
        }
    }

    fn resolve<'s>(&'s self, schema: &'s Value) -> Result<Resolved<'s>, String> {
        let mut layers = Vec::new();
        let mut cur = schema;
        for _ in 0..16 {
            // a schema that is not an object, or has no reference, ends the walk (what it is not an object adds no keys)
            let Some(map) = cur.as_object() else { return Ok(Resolved { layers }) };
            layers.push(map);
            let Some(r) = map.get("$ref").and_then(Value::as_str) else { return Ok(Resolved { layers }) };
            let name = r
                .strip_prefix("#/definitions/")
                .ok_or_else(|| format!("the type reference '{r}' is not valid; it must look like '#/definitions/name'."))?;
            cur = self
                .definitions
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, d)| d)
                .ok_or_else(|| format!("the type definition '{name}' is not found in the 'definitions' of the template."))?;
        }
        Err("the type definitions refer to each other in a loop.".into())
    }

    /// The declared type name (`string`, `int`, ...) of a schema, lower case; empty when it has none.
    pub fn type_of(&self, schema: &Value) -> Result<String, String> {
        Ok(self.resolve(schema)?.get("type").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase())
    }

    pub fn nullable(&self, schema: &Value) -> bool {
        self.resolve(schema).ok().and_then(|s| s.get("nullable").and_then(Value::as_bool)).unwrap_or(false)
    }

    /// Check `v` against `schema`; `Err` says what is wrong and where (`properties.name`).
    pub fn check(&self, schema: &Value, v: &Value) -> Result<(), String> {
        self.check_at(schema, v, "", 0)
    }

    fn check_at(&self, schema: &Value, v: &Value, path: &str, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err(format!("{}the value is nested too deeply.", at(path)));
        }
        self.charge(NODE_COST)?;
        let s = self.resolve(schema)?;
        if v.is_null() {
            return if s.get("nullable").and_then(Value::as_bool).unwrap_or(false) {
                Ok(())
            } else {
                Err(format!("{}the value is null, but the type is not nullable.", at(path)))
            };
        }
        let ty = s.get("type").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let ok = match ty.as_str() {
            "string" | "securestring" => v.is_string(),
            "int" => v.as_i64().is_some() || v.as_u64().is_some(),
            "bool" => v.is_boolean(),
            "object" | "secureobject" => v.is_object(),
            "array" => v.is_array(),
            "" => true,
            other => return Err(format!("{}the type '{other}' is not known.", at(path))),
        };
        if !ok {
            return Err(format!("{}expected {ty}, got {}.", at(path), type_name(v)));
        }
        if let Some(allowed) = s.get("allowedValues").and_then(Value::as_array) {
            self.charge(allowed.len())?;
            let hit = allowed.iter().any(|a| match (a, v) {
                (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y),
                _ => a == v,
            });
            if !hit {
                return Err(format!("{}the value must be one of {}.", at(path), Value::Array(allowed.clone())));
            }
        }
        if let Some(n) = v.as_i64() {
            if s.get("minValue").and_then(Value::as_i64).is_some_and(|m| n < m)
                || s.get("maxValue").and_then(Value::as_i64).is_some_and(|m| n > m)
            {
                return Err(format!("{}the value {n} is out of range.", at(path)));
            }
        }
        let len = match v {
            Value::String(t) => {
                self.charge(t.len())?;
                Some(t.chars().count() as i64)
            }
            Value::Array(a) => Some(a.len() as i64),
            _ => None,
        };
        if let Some(len) = len {
            if s.get("minLength").and_then(Value::as_i64).is_some_and(|m| len < m)
                || s.get("maxLength").and_then(Value::as_i64).is_some_and(|m| len > m)
            {
                return Err(format!("{}the length {len} is not allowed.", at(path)));
            }
        }
        if let Value::Array(items) = v {
            let prefix: &[Value] = s.get("prefixItems").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            for (i, x) in items.iter().enumerate() {
                let p = format!("{path}[{i}]");
                match prefix.get(i) {
                    Some(sch) => self.check_at(sch, x, &p, depth + 1)?,
                    None => match s.get("items") {
                        Some(Value::Bool(false)) => return Err(format!("{}no more than {} item(s) are allowed.", at(path), prefix.len())),
                        Some(sch @ Value::Object(_)) => self.check_at(sch, x, &p, depth + 1)?,
                        _ => {}
                    },
                }
            }
        }
        if let Value::Object(o) = v {
            let props = s.get("properties").and_then(Value::as_object);
            self.charge(o.len() + props.map_or(0, Map::len))?;
            if let Some(props) = props {
                // the names of the value, by lower case, made only when a name is not found as it is written in the schema
                let mut by_lower: Option<HashMap<String, &Value>> = None;
                for (k, sch) in props {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    let found = match o.get(k) {
                        Some(x) => Some(x),
                        None => by_lower
                            .get_or_insert_with(|| {
                                let mut m = HashMap::new();
                                for (name, x) in o {
                                    // the first of the names that differ only in case is the one found, as a scan would
                                    m.entry(name.to_ascii_lowercase()).or_insert(x);
                                }
                                m
                            })
                            .get(&k.to_ascii_lowercase())
                            .copied(),
                    };
                    match found {
                        Some(x) => self.check_at(sch, x, &p, depth + 1)?,
                        None if self.nullable(sch) => {}
                        None => return Err(format!("{}the property '{k}' is required.", at(path))),
                    }
                }
            }
            let extra = s.get("additionalProperties");
            if matches!(extra, Some(Value::Bool(false)) | Some(Value::Object(_))) {
                let named: HashSet<String> = props.map(|p| p.keys().map(|n| n.to_ascii_lowercase()).collect()).unwrap_or_default();
                for (k, x) in o {
                    if named.contains(&k.to_ascii_lowercase()) {
                        continue;
                    }
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    match extra {
                        Some(Value::Bool(false)) => return Err(format!("{}the property '{k}' is not allowed.", at(path))),
                        Some(sch @ Value::Object(_)) => self.check_at(sch, x, &p, depth + 1)?,
                        _ => {}
                    }
                }
            }
            if let Some(d) = s.get("discriminator").and_then(Value::as_object) {
                let prop = d.get("propertyName").and_then(Value::as_str).unwrap_or("");
                let tag = o.iter().find(|(k, _)| k.eq_ignore_ascii_case(prop)).map(|(_, x)| x.as_str().unwrap_or(""));
                let map = d.get("mapping").and_then(Value::as_object);
                self.charge(map.map_or(0, Map::len))?;
                match (tag, map) {
                    (Some(t), Some(m)) => match m.iter().find(|(k, _)| k.eq_ignore_ascii_case(t)) {
                        Some((_, sch)) => self.check_at(sch, v, path, depth + 1)?,
                        None => return Err(format!("{}the value '{t}' of '{prop}' is not one of the discriminator's choices.", at(path))),
                    },
                    _ => return Err(format!("{}the property '{prop}' that tells the kind of the object is missing.", at(path))),
                }
            }
        }
        Ok(())
    }
}

fn at(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}: ")
    }
}

/// A value given for a parameter, turned into the declared type when the template language does it
/// (the number 5 for a string parameter, the text "5" for an int, "true" for a bool).
pub fn coerce(ty: &str, v: Value) -> Result<Value, String> {
    match (ty, &v) {
        ("string" | "securestring", Value::String(_)) => Ok(v),
        ("string" | "securestring", Value::Number(n)) => Ok(Value::String(n.to_string())),
        ("int", Value::Number(_)) => Ok(v),
        ("int", Value::String(t)) => t.trim().parse::<i64>().map(|n| json!(n)).map_err(|_| format!("'{t}' is not an integer")),
        ("bool", Value::Bool(_)) => Ok(v),
        ("bool", Value::String(t)) if t.eq_ignore_ascii_case("true") => Ok(json!(true)),
        ("bool", Value::String(t)) if t.eq_ignore_ascii_case("false") => Ok(json!(false)),
        ("array", Value::Array(_)) => Ok(v),
        ("object" | "secureobject", Value::Object(_)) => Ok(v),
        (t, other) => Err(format!("expected {t}, got {}", type_name(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> Map<String, Value> {
        json!({
            "queueSetting": {"type": "object", "properties": {"name": {"type": "string"}, "maxDelivery": {"type": "int", "nullable": true, "minValue": 1}}},
            "tree": {"type": "object", "properties": {"kids": {"type": "array", "nullable": true, "items": {"$ref": "#/definitions/tree"}}}}
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    }

    #[test]
    fn user_defined_types_as_bicep_writes_them() {
        let d = defs();
        let t = Types::new(&d);
        let schema = json!({"type": "array", "items": {"$ref": "#/definitions/queueSetting"}});
        assert!(t.check(&schema, &json!([{"name": "a", "maxDelivery": 3}, {"name": "b"}])).is_ok());
        let e = t.check(&schema, &json!([{"name": "a", "maxDelivery": 0}])).unwrap_err();
        assert!(e.contains("[0].maxDelivery") && e.contains("out of range"), "{e}");
        let e = t.check(&schema, &json!([{"maxDelivery": 3}])).unwrap_err();
        assert!(e.contains("'name' is required"), "{e}");
        let e = t.check(&schema, &json!([{"name": 5}])).unwrap_err();
        assert!(e.contains("expected string"), "{e}");
        assert!(t.check(&json!({"$ref": "#/definitions/nope"}), &json!(1)).unwrap_err().contains("nope"));
    }

    #[test]
    fn nullable_and_recursive_types() {
        let d = defs();
        let t = Types::new(&d);
        assert!(t.check(&json!({"type": "string", "nullable": true}), &Value::Null).is_ok());
        assert!(t.check(&json!({"type": "string"}), &Value::Null).is_err());
        assert!(t.check(&json!({"$ref": "#/definitions/tree", "nullable": true}), &Value::Null).is_ok());
        assert!(t.check(&json!({"$ref": "#/definitions/tree"}), &json!({"kids": [{"kids": [{}]}, {}]})).is_ok());
        assert!(t.check(&json!({"$ref": "#/definitions/tree"}), &json!({"kids": [{"kids": 3}]})).is_err());
    }

    #[test]
    fn constraints() {
        let d = Map::new();
        let t = Types::new(&d);
        assert!(t.check(&json!({"type": "string", "allowedValues": ["A", "b"]}), &json!("a")).is_ok());
        assert!(t.check(&json!({"type": "string", "allowedValues": ["A", "b"]}), &json!("c")).is_err());
        assert!(t.check(&json!({"type": "string", "minLength": 2, "maxLength": 3}), &json!("abcd")).is_err());
        assert!(t.check(&json!({"type": "array", "prefixItems": [{"type": "int"}], "items": false}), &json!([1, 2])).is_err());
        assert!(t
            .check(&json!({"type": "array", "prefixItems": [{"type": "int"}], "items": {"type": "string"}}), &json!([1, "x"]))
            .is_ok());
        assert!(t
            .check(
                &json!({"type": "object", "properties": {"a": {"type": "int"}}, "additionalProperties": false}),
                &json!({"a": 1, "b": 2})
            )
            .is_err());
        let disc = json!({"type": "object", "discriminator": {"propertyName": "kind", "mapping": {"a": {"type": "object", "properties": {"x": {"type": "int"}}}}}});
        assert!(t.check(&disc, &json!({"kind": "a", "x": 1})).is_ok());
        assert!(t.check(&disc, &json!({"kind": "a", "x": "no"})).is_err());
        assert!(t.check(&disc, &json!({"kind": "z"})).is_err());
    }

    #[test]
    fn coercion_of_given_values() {
        assert_eq!(coerce("string", json!(5)).ok(), Some(json!("5")));
        assert_eq!(coerce("int", json!("7")).ok(), Some(json!(7)));
        assert_eq!(coerce("bool", json!("TRUE")).ok(), Some(json!(true)));
        assert!(coerce("array", json!({})).is_err());
    }

    #[test]
    fn a_schema_and_a_value_of_twenty_thousand_entries_are_checked_in_linear_time() {
        let d = Map::new();
        let budget = Budget::default();
        let t = Types::new(&d).with_budget(&budget);
        // 20,000 properties in the schema, 20,000 in the value, with the names in another case: no name is found as it is written
        let n = 20_000;
        let props: Map<String, Value> = (0..n).map(|i| (format!("p{i}"), json!({"type": "int"}))).collect();
        let schema = json!({"type": "object", "properties": props, "additionalProperties": false});
        let value: Map<String, Value> = (0..n).rev().map(|i| (format!("P{i}"), json!(i))).collect();
        let started = std::time::Instant::now();
        assert!(t.check(&schema, &Value::Object(value.clone())).is_ok());
        let mut bad = value;
        bad.insert("extra".into(), json!(1));
        assert!(t.check(&schema, &Value::Object(bad)).unwrap_err().contains("'extra' is not allowed"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn work_is_charged_to_the_budget_and_a_check_that_needs_too_much_is_refused() {
        let d = Map::new();
        let budget = Budget::default();
        let t = Types::new(&d).with_budget(&budget);
        // 20,000 values, each looked up in 20,000 allowed values: 400 million comparisons
        let allowed: Vec<Value> = (0..20_000).map(|i| json!(format!("v{i}"))).collect();
        let schema = json!({"type": "array", "items": {"type": "string", "allowedValues": allowed}});
        let value = Value::Array((0..20_000).map(|i| json!(format!("v{}", 19_999 - i))).collect());
        let started = std::time::Instant::now();
        let e = t.check(&schema, &value).unwrap_err();
        assert!(e.contains("too complex"), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
        // a small check is not refused, and without a budget nothing is charged
        assert!(Types::new(&d)
            .with_budget(&Budget::default())
            .check(&json!({"type": "array", "items": {"type": "int"}}), &json!([1, 2, 3]))
            .is_ok());
        assert!(Types::new(&d).check(&json!({"type": "string", "allowedValues": ["a"]}), &json!("A")).is_ok());
    }

    #[test]
    fn references_are_followed_without_changing_what_the_schema_says() {
        let defs = json!({
            "base": {"type": "string", "minLength": 2},
            "alias": {"$ref": "#/definitions/base", "maxLength": 3},
            "nullableAlias": {"$ref": "#/definitions/alias", "nullable": true}
        });
        let d = defs.as_object().cloned().unwrap_or_default();
        let t = Types::new(&d);
        let s = json!({"$ref": "#/definitions/nullableAlias"});
        assert!(t.check(&s, &Value::Null).is_ok());
        assert!(t.check(&s, &json!("abc")).is_ok());
        assert!(t.check(&s, &json!("a")).is_err());
        assert!(t.check(&s, &json!("abcd")).is_err());
        // the keys of the outer schema win over the ones it refers to
        assert!(t.check(&json!({"$ref": "#/definitions/alias", "maxLength": 5}), &json!("abcde")).is_ok());
        assert_eq!(t.type_of(&s).unwrap(), "string");
        let looped = json!({"a": {"$ref": "#/definitions/b"}, "b": {"$ref": "#/definitions/a"}}).as_object().cloned().unwrap_or_default();
        assert!(Types::new(&looped).check(&json!({"$ref": "#/definitions/a"}), &json!(1)).unwrap_err().contains("loop"));
    }
}
