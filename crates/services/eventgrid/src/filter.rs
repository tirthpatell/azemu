//! Subscription filters: event types, subject prefix/suffix, and the advanced filters
//! on `data.` paths.
//!
//! Rules taken from the Event Grid documentation:
//! * all conditions are ANDed; the values of one `...In` filter are ORed;
//! * string comparisons are case-insensitive (the subject filters unless
//!   `isSubjectCaseSensitive` is true);
//! * a key that is not in the event makes the filter false, except `IsNullOrUndefined`
//!   (true) and the negative operators (`StringNotIn`, `NumberNotIn`, ... true);
//! * a key of the wrong type (a number for `StringIn`) makes the filter false.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::Event;

pub const MAX_ADVANCED_FILTERS: usize = 25;
pub const MAX_FILTER_VALUES: usize = 25;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_event_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_begins_with: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_ends_with: Option<String>,
    #[serde(default)]
    pub is_subject_case_sensitive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "filters")]
    pub advanced_filters: Vec<AdvancedFilter>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvancedFilter {
    pub operator_type: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
}

/// The operators, parsed from `operatorType` (case-insensitive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    NumberIn,
    NumberNotIn,
    NumberLessThan,
    NumberGreaterThan,
    NumberLessThanOrEquals,
    NumberGreaterThanOrEquals,
    NumberInRange,
    NumberNotInRange,
    BoolEquals,
    StringIn,
    StringNotIn,
    StringBeginsWith,
    StringNotBeginsWith,
    StringEndsWith,
    StringNotEndsWith,
    StringContains,
    StringNotContains,
    IsNullOrUndefined,
    IsNotNull,
}

impl Op {
    fn parse(s: &str) -> Option<Op> {
        Some(match s.to_ascii_lowercase().as_str() {
            "numberin" => Op::NumberIn,
            "numbernotin" => Op::NumberNotIn,
            "numberlessthan" => Op::NumberLessThan,
            "numbergreaterthan" => Op::NumberGreaterThan,
            "numberlessthanorequals" => Op::NumberLessThanOrEquals,
            "numbergreaterthanorequals" => Op::NumberGreaterThanOrEquals,
            "numberinrange" => Op::NumberInRange,
            "numbernotinrange" => Op::NumberNotInRange,
            "boolequals" => Op::BoolEquals,
            "stringin" => Op::StringIn,
            "stringnotin" => Op::StringNotIn,
            "stringbeginswith" => Op::StringBeginsWith,
            "stringnotbeginswith" => Op::StringNotBeginsWith,
            "stringendswith" => Op::StringEndsWith,
            "stringnotendswith" => Op::StringNotEndsWith,
            "stringcontains" => Op::StringContains,
            "stringnotcontains" => Op::StringNotContains,
            "isnullorundefined" => Op::IsNullOrUndefined,
            "isnotnull" => Op::IsNotNull,
            _ => return None,
        })
    }

    fn negative(self) -> bool {
        matches!(
            self,
            Op::NumberNotIn
                | Op::NumberNotInRange
                | Op::StringNotIn
                | Op::StringNotBeginsWith
                | Op::StringNotEndsWith
                | Op::StringNotContains
        )
    }

    fn takes_value(self) -> bool {
        matches!(
            self,
            Op::NumberLessThan | Op::NumberGreaterThan | Op::NumberLessThanOrEquals | Op::NumberGreaterThanOrEquals | Op::BoolEquals
        )
    }

    fn takes_nothing(self) -> bool {
        matches!(self, Op::IsNullOrUndefined | Op::IsNotNull)
    }
}

impl Filter {
    /// Check the filter when a subscription is created. `Err` is a message for a 400.
    pub fn validate(&self) -> Result<(), String> {
        if self.advanced_filters.len() > MAX_ADVANCED_FILTERS {
            return Err(format!("A maximum of {MAX_ADVANCED_FILTERS} advanced filters are allowed per subscription."));
        }
        for f in &self.advanced_filters {
            f.validate()?;
        }
        Ok(())
    }

    /// Does the event pass every condition?
    pub fn matches(&self, ev: &Event) -> bool {
        if !self.included_event_types.is_empty() {
            let Some(t) = ev.event_type() else { return false };
            if !self.included_event_types.iter().any(|w| w == "All" || w.eq_ignore_ascii_case(t)) {
                return false;
            }
        }
        if self.subject_begins_with.is_some() || self.subject_ends_with.is_some() {
            let subject = ev.subject().unwrap_or("");
            if let Some(p) = &self.subject_begins_with {
                if !subject_cmp(subject, p, self.is_subject_case_sensitive, |s, p| s.starts_with(p)) {
                    return false;
                }
            }
            if let Some(p) = &self.subject_ends_with {
                if !subject_cmp(subject, p, self.is_subject_case_sensitive, |s, p| s.ends_with(p)) {
                    return false;
                }
            }
        }
        self.advanced_filters.iter().all(|f| f.matches(ev))
    }
}

fn subject_cmp(subject: &str, pattern: &str, case_sensitive: bool, f: impl Fn(&str, &str) -> bool) -> bool {
    if case_sensitive {
        f(subject, pattern)
    } else {
        f(&subject.to_lowercase(), &pattern.to_lowercase())
    }
}

impl AdvancedFilter {
    fn op(&self) -> Result<Op, String> {
        Op::parse(&self.operator_type).ok_or_else(|| format!("Unsupported advanced filter operator '{}'.", self.operator_type))
    }

    fn operands(&self) -> Vec<&Value> {
        let mut v: Vec<&Value> = self.values.iter().collect();
        if v.is_empty() {
            if let Some(x) = &self.value {
                v.push(x);
            }
        }
        v
    }

    pub fn validate(&self) -> Result<(), String> {
        let op = self.op()?;
        if self.key.trim().is_empty() {
            return Err(format!("The advanced filter '{}' has no key.", self.operator_type));
        }
        let vals = self.operands();
        if vals.len() > MAX_FILTER_VALUES {
            return Err(format!("The advanced filter '{}' has more than {MAX_FILTER_VALUES} values.", self.operator_type));
        }
        if op.takes_nothing() {
            return Ok(());
        }
        if vals.is_empty() {
            return Err(format!("The advanced filter '{}' on '{}' needs a value.", self.operator_type, self.key));
        }
        if op.takes_value() && self.value.is_none() && vals.len() != 1 {
            return Err(format!("The advanced filter '{}' takes a single value.", self.operator_type));
        }
        let bad = |what: &str| Err(format!("The advanced filter '{}' on '{}' needs {what}.", self.operator_type, self.key));
        match op {
            Op::NumberIn
            | Op::NumberNotIn
            | Op::NumberLessThan
            | Op::NumberGreaterThan
            | Op::NumberLessThanOrEquals
            | Op::NumberGreaterThanOrEquals => {
                if vals.iter().any(|v| !v.is_number()) {
                    return bad("number values");
                }
            }
            Op::NumberInRange | Op::NumberNotInRange => {
                if vals.iter().any(|v| range(v).is_none()) {
                    return bad("[low, high] number pairs");
                }
            }
            Op::BoolEquals => {
                if vals.iter().any(|v| !v.is_boolean()) {
                    return bad("a boolean value");
                }
            }
            Op::StringIn
            | Op::StringNotIn
            | Op::StringBeginsWith
            | Op::StringNotBeginsWith
            | Op::StringEndsWith
            | Op::StringNotEndsWith
            | Op::StringContains
            | Op::StringNotContains => {
                if vals.iter().any(|v| !v.is_string()) {
                    return bad("string values");
                }
            }
            Op::IsNullOrUndefined | Op::IsNotNull => {}
        }
        Ok(())
    }

    pub fn matches(&self, ev: &Event) -> bool {
        let Ok(op) = self.op() else { return false };
        let found = ev.lookup(&self.key);
        match op {
            Op::IsNullOrUndefined => return matches!(found, None | Some(Value::Null)),
            Op::IsNotNull => return !matches!(found, None | Some(Value::Null)),
            _ => {}
        }
        let Some(v) = found else { return op.negative() };
        let vals = self.operands();
        match op {
            Op::NumberIn | Op::NumberNotIn => {
                let Some(n) = v.as_f64() else { return false };
                let hit = vals.iter().filter_map(|x| x.as_f64()).any(|x| x == n);
                hit != (op == Op::NumberNotIn)
            }
            Op::NumberLessThan | Op::NumberGreaterThan | Op::NumberLessThanOrEquals | Op::NumberGreaterThanOrEquals => {
                let (Some(n), Some(x)) = (v.as_f64(), vals.first().and_then(|x| x.as_f64())) else {
                    return false;
                };
                match op {
                    Op::NumberLessThan => n < x,
                    Op::NumberGreaterThan => n > x,
                    Op::NumberLessThanOrEquals => n <= x,
                    _ => n >= x,
                }
            }
            Op::NumberInRange | Op::NumberNotInRange => {
                let Some(n) = v.as_f64() else { return false };
                let hit = vals.iter().filter_map(|x| range(x)).any(|(lo, hi)| n >= lo && n <= hi);
                hit != (op == Op::NumberNotInRange)
            }
            Op::BoolEquals => match (v.as_bool(), vals.first().and_then(|x| x.as_bool())) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
            _ => {
                let Some(s) = v.as_str() else { return false };
                let s = s.to_lowercase();
                let texts: Vec<String> = vals.iter().filter_map(|x| x.as_str()).map(str::to_lowercase).collect();
                match op {
                    Op::StringIn => texts.contains(&s),
                    Op::StringNotIn => !texts.contains(&s),
                    Op::StringBeginsWith => texts.iter().any(|t| s.starts_with(t.as_str())),
                    Op::StringNotBeginsWith => !texts.iter().any(|t| s.starts_with(t.as_str())),
                    Op::StringEndsWith => texts.iter().any(|t| s.ends_with(t.as_str())),
                    Op::StringNotEndsWith => !texts.iter().any(|t| s.ends_with(t.as_str())),
                    Op::StringContains => texts.iter().any(|t| s.contains(t.as_str())),
                    Op::StringNotContains => !texts.iter().any(|t| s.contains(t.as_str())),
                    _ => false,
                }
            }
        }
    }
}

fn range(v: &Value) -> Option<(f64, f64)> {
    let a = v.as_array()?;
    if a.len() != 2 {
        return None;
    }
    Some((a[0].as_f64()?, a[1].as_f64()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Schema;
    use serde_json::json;

    fn ev() -> Event {
        Event::new(
            Schema::EventGrid,
            json!({"id":"1","subject":"/blobServices/default/containers/Photos/blobs/a.PNG","eventType":"Contoso.Created",
                   "dataVersion":"1","eventTime":"2024-01-01T00:00:00Z",
                   "data":{"color":"Red","n":5,"f":2.5,"ok":true,"nothing":null,"tags":["a"],"obj":{"k":"v"}}}),
        )
    }

    fn adv(op: &str, key: &str, values: Value) -> Filter {
        let mut f = json!({"operatorType": op, "key": key});
        match values {
            Value::Array(_) => f["values"] = values,
            v => f["value"] = v,
        }
        Filter { advanced_filters: vec![serde_json::from_value(f).unwrap()], ..Filter::default() }
    }

    fn check(op: &str, key: &str, values: Value) -> bool {
        let f = adv(op, key, values);
        f.validate().unwrap();
        f.matches(&ev())
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(Filter::default().matches(&ev()));
    }

    #[test]
    fn event_types() {
        let f = |t: &[&str]| Filter { included_event_types: t.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        assert!(f(&["Contoso.Created"]).matches(&ev()));
        assert!(f(&["other", "contoso.created"]).matches(&ev()));
        assert!(!f(&["other"]).matches(&ev()));
        assert!(f(&["All"]).matches(&ev()));
    }

    #[test]
    fn subject_prefix_suffix_and_case() {
        let f = Filter { subject_begins_with: Some("/blobservices/".into()), subject_ends_with: Some(".png".into()), ..Default::default() };
        assert!(f.matches(&ev()));
        let f = Filter { is_subject_case_sensitive: true, ..f };
        assert!(!f.matches(&ev()));
        let f = Filter {
            subject_begins_with: Some("/blobServices/".into()),
            subject_ends_with: Some(".PNG".into()),
            is_subject_case_sensitive: true,
            ..Default::default()
        };
        assert!(f.matches(&ev()));
        let f = Filter { subject_begins_with: Some("/x".into()), ..Default::default() };
        assert!(!f.matches(&ev()));
    }

    #[test]
    fn string_in_and_not_in() {
        assert!(check("StringIn", "data.color", json!(["blue", "RED"])));
        assert!(!check("StringIn", "data.color", json!(["blue"])));
        assert!(!check("StringIn", "data.n", json!(["5"])));
        assert!(check("StringNotIn", "data.color", json!(["blue"])));
        assert!(!check("StringNotIn", "data.color", json!(["red"])));
        assert!(check("StringNotIn", "data.absent", json!(["red"])));
        assert!(!check("StringIn", "data.absent", json!(["red"])));
    }

    #[test]
    fn string_contains_begins_ends() {
        assert!(check("StringContains", "data.color", json!(["ed"])));
        assert!(!check("StringContains", "data.color", json!(["zz"])));
        assert!(check("StringNotContains", "data.color", json!(["zz"])));
        assert!(check("StringBeginsWith", "data.color", json!(["r"])));
        assert!(check("StringNotBeginsWith", "data.color", json!(["b"])));
        assert!(!check("StringNotBeginsWith", "data.color", json!(["re"])));
        assert!(check("StringEndsWith", "data.color", json!(["D"])));
        assert!(check("StringNotEndsWith", "data.color", json!(["x"])));
        assert!(check("StringEndsWith", "subject", json!([".png"])));
    }

    #[test]
    fn numbers() {
        assert!(check("NumberIn", "data.n", json!([1, 5])));
        assert!(!check("NumberIn", "data.n", json!([1, 2])));
        assert!(check("NumberNotIn", "data.n", json!([1, 2])));
        assert!(!check("NumberNotIn", "data.n", json!([5])));
        assert!(check("NumberGreaterThan", "data.n", json!(4)));
        assert!(!check("NumberGreaterThan", "data.n", json!(5)));
        assert!(check("NumberGreaterThanOrEquals", "data.n", json!(5)));
        assert!(check("NumberLessThan", "data.n", json!(6)));
        assert!(!check("NumberLessThan", "data.n", json!(5)));
        assert!(check("NumberLessThanOrEquals", "data.n", json!(5)));
        assert!(check("NumberLessThan", "data.f", json!(2.6)));
        assert!(check("NumberInRange", "data.n", json!([[1, 3], [4, 6]])));
        assert!(!check("NumberInRange", "data.n", json!([[1, 3]])));
        assert!(check("NumberNotInRange", "data.n", json!([[1, 3]])));
        assert!(!check("NumberGreaterThan", "data.color", json!(1)));
        assert!(!check("NumberGreaterThan", "data.absent", json!(1)));
    }

    #[test]
    fn bools() {
        assert!(check("BoolEquals", "data.ok", json!(true)));
        assert!(!check("BoolEquals", "data.ok", json!(false)));
        assert!(!check("BoolEquals", "data.n", json!(true)));
        assert!(!check("BoolEquals", "data.absent", json!(true)));
    }

    #[test]
    fn null_checks() {
        let f = |op, key: &str| {
            let f = Filter {
                advanced_filters: vec![AdvancedFilter { operator_type: op, key: key.into(), value: None, values: vec![] }],
                ..Default::default()
            };
            f.validate().unwrap();
            f.matches(&ev())
        };
        assert!(f("IsNullOrUndefined".into(), "data.absent"));
        assert!(f("IsNullOrUndefined".into(), "data.nothing"));
        assert!(!f("IsNullOrUndefined".into(), "data.color"));
        assert!(f("IsNotNull".into(), "data.color"));
        assert!(f("IsNotNull".into(), "data.obj"));
        assert!(!f("IsNotNull".into(), "data.nothing"));
        assert!(!f("IsNotNull".into(), "data.absent"));
    }

    #[test]
    fn keys_nested_and_top_level() {
        assert!(check("StringIn", "data.obj.k", json!(["v"])));
        assert!(check("StringIn", "eventType", json!(["contoso.created"])));
        assert!(check("StringIn", "id", json!(["1"])));
        assert!(check("StringIn", "data.tags[0]", json!(["a"])));
    }

    #[test]
    fn all_filters_must_match() {
        let f = Filter {
            advanced_filters: vec![
                serde_json::from_value(json!({"operatorType":"StringIn","key":"data.color","values":["red"]})).unwrap(),
                serde_json::from_value(json!({"operatorType":"NumberGreaterThan","key":"data.n","value":100})).unwrap(),
            ],
            ..Default::default()
        };
        assert!(!f.matches(&ev()));
    }

    #[test]
    fn validation_errors() {
        let bad = |v: Value| Filter { advanced_filters: vec![serde_json::from_value(v).unwrap()], ..Default::default() }.validate();
        assert!(bad(json!({"operatorType":"Nope","key":"k","values":["a"]})).is_err());
        assert!(bad(json!({"operatorType":"StringIn","key":"","values":["a"]})).is_err());
        assert!(bad(json!({"operatorType":"StringIn","key":"k"})).is_err());
        assert!(bad(json!({"operatorType":"StringIn","key":"k","values":[1]})).is_err());
        assert!(bad(json!({"operatorType":"NumberIn","key":"k","values":["a"]})).is_err());
        assert!(bad(json!({"operatorType":"NumberInRange","key":"k","values":[1]})).is_err());
        assert!(bad(json!({"operatorType":"BoolEquals","key":"k","value":"x"})).is_err());
        assert!(bad(json!({"operatorType":"IsNotNull","key":"k"})).is_ok());
        let many: Vec<Value> = (0..26).map(|_| json!({"operatorType":"IsNotNull","key":"k"})).collect();
        let f = Filter { advanced_filters: serde_json::from_value(Value::Array(many)).unwrap(), ..Default::default() };
        assert!(f.validate().is_err());
    }

    #[test]
    fn json_shape_roundtrip_accepts_namespace_name() {
        let f: Filter = serde_json::from_value(json!({
            "includedEventTypes": ["A"],
            "filters": [{"operatorType":"StringIn","key":"data.x","values":["y"]}]
        }))
        .unwrap();
        assert_eq!(f.advanced_filters.len(), 1);
    }
}
