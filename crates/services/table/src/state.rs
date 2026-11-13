//! What the service remembers: accounts, tables, entities, access policies.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;

use crate::filter::{eval, Expr, Lookup};
use crate::props::ServiceProps;
use crate::values::{format_ticks, Ticks, Value};

/// A string that sorts by UTF-16 code units, which is how Table Storage orders
/// keys (it differs from `str` order only for characters above U+FFFF).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OKey(pub String);

impl Ord for OKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.encode_utf16().cmp(other.0.encode_utf16())
    }
}

impl PartialOrd for OKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub type Key = (OKey, OKey);

pub fn key(pk: &str, rk: &str) -> Key {
    (OKey(pk.to_string()), OKey(rk.to_string()))
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entity {
    pub pk: String,
    pub rk: String,
    pub props: Vec<(String, Value)>,
    pub ts: Ticks,
}

impl Entity {
    pub fn etag(&self) -> String {
        etag_for(self.ts)
    }

    pub fn prop(&self, name: &str) -> Option<&Value> {
        self.props.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub fn size(&self) -> usize {
        (self.pk.len() + self.rk.len()) * 2 + self.props.iter().map(|(k, v)| k.len() * 2 + v.size()).sum::<usize>()
    }
}

/// `W/"datetime'2022-10-27T22%3A06%3A34.6827829Z'"`
pub fn etag_for(ts: Ticks) -> String {
    format!("W/\"datetime'{}'\"", format_ticks(ts).replace(':', "%3A"))
}

impl Lookup for Entity {
    fn get(&self, name: &str) -> Option<Cow<'_, Value>> {
        match name {
            "PartitionKey" => Some(Cow::Owned(Value::Str(self.pk.clone()))),
            "RowKey" => Some(Cow::Owned(Value::Str(self.rk.clone()))),
            "Timestamp" => Some(Cow::Owned(Value::DateTime(self.ts))),
            _ => self.prop(name).map(Cow::Borrowed),
        }
    }
}

/// A stored access policy (one `SignedIdentifier`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SignedId {
    pub id: String,
    /// `None` when the identifier was stored without an `AccessPolicy`.
    pub policy: Option<Policy>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    /// Normalised to `2021-06-08T02:05:09.0000000Z`.
    pub start: Option<String>,
    pub expiry: Option<String>,
    pub permission: Option<String>,
}

#[derive(Debug, Default)]
pub struct Table {
    /// The name as it was created (case is kept, lookups ignore it).
    pub name: String,
    pub entities: BTreeMap<Key, Entity>,
    pub acl: Vec<SignedId>,
}

pub struct Page<'a> {
    pub items: Vec<&'a Entity>,
    /// Key of the first matching entity that did not fit on this page.
    pub next: Option<(String, String)>,
}

impl Table {
    /// Matching entities in key order, starting at `from` (inclusive), at most `top`.
    /// `allow` filters by something other than the OData filter (SAS key ranges).
    pub fn query<'a>(&'a self, filter: Option<&Expr>, from: Option<(&str, &str)>, top: usize, allow: &dyn Fn(&Entity) -> bool) -> Page<'a> {
        let lower = match from {
            Some((p, r)) => Bound::Included(key(p, r)),
            None => Bound::Unbounded,
        };
        let mut items = Vec::new();
        let mut next = None;
        for (_, e) in self.entities.range((lower, Bound::Unbounded)) {
            if !allow(e) || !filter.map(|f| eval(f, e)).unwrap_or(true) {
                continue;
            }
            if items.len() == top {
                next = Some((e.pk.clone(), e.rk.clone()));
                break;
            }
            items.push(e);
        }
        Page { items, next }
    }
}

#[derive(Debug, Default)]
pub struct Account {
    /// Keyed by lower-case table name.
    pub tables: BTreeMap<String, Table>,
    pub props: ServiceProps,
}

#[derive(Debug, Default)]
pub struct State {
    pub accounts: HashMap<String, Account>,
}

impl State {
    pub fn account(&mut self, name: &str) -> &mut Account {
        self.accounts.entry(name.to_string()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn okey_orders_by_utf16_units() {
        assert!(OKey("\u{fffd}".into()) > OKey("\u{1f642}".into()));
        assert!(OKey("Z".into()) < OKey("a".into()));
        assert!(OKey("".into()) < OKey("a".into()));
        assert!(OKey("10".into()) < OKey("9".into()));
    }

    #[test]
    fn etag_shape() {
        assert_eq!(etag_for(0), "W/\"datetime'1970-01-01T00%3A00%3A00Z'\"");
    }

    #[test]
    fn query_pages_and_next_key() {
        let mut t = Table::default();
        for i in 0..5 {
            let e = Entity { pk: "p".into(), rk: format!("{i}"), props: vec![], ts: 0 };
            t.entities.insert(key("p", &format!("{i}")), e);
        }
        let all = |_: &Entity| true;
        let p = t.query(None, None, 2, &all);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.next, Some(("p".into(), "2".into())));
        let p = t.query(None, Some(("p", "2")), 3, &all);
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.next, None);
    }
}
