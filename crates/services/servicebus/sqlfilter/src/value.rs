//! Values, property scopes and the property-access traits.

use std::collections::BTreeMap;

/// A runtime value. `Null` behaves like a missing property ("unknown").
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Str(String),
    /// Milliseconds since the Unix epoch.
    Timestamp(i64),
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Long(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Long(i64::from(v))
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Double(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}

/// Which property bag a name refers to. An unscoped name means `User`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Sys,
    User,
}

/// Read access to a message's properties.
///
/// Property names are case-insensitive in Service Bus. The filter passes
/// the name exactly as written (without brackets or quotes) and the
/// implementor does the case-insensitive lookup.
pub trait Properties {
    /// `None` = the property does not exist.
    fn get(&self, scope: Scope, name: &str) -> Option<Value>;
}

/// Write access, used by rule actions.
pub trait PropertiesMut: Properties {
    fn set(&mut self, scope: Scope, name: &str, value: Value);
    fn remove(&mut self, scope: Scope, name: &str);
}

/// Rule parameters; keys are written without the leading `@`.
pub type Params = BTreeMap<String, Value>;

/// A simple in-memory property bag with case-insensitive names. Handy for
/// tests and for callers that do not have their own message type.
#[derive(Debug, Clone, Default)]
pub struct MapProperties {
    map: BTreeMap<(bool, String), (String, Value)>,
}

fn key(scope: Scope, name: &str) -> (bool, String) {
    (scope == Scope::Sys, name.to_lowercase())
}

impl MapProperties {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style insert.
    pub fn with(mut self, scope: Scope, name: &str, value: impl Into<Value>) -> Self {
        PropertiesMut::set(&mut self, scope, name, value.into());
        self
    }

    /// Builder-style insert into the user scope.
    pub fn user(self, name: &str, value: impl Into<Value>) -> Self {
        self.with(Scope::User, name, value)
    }

    /// Builder-style insert into the system scope.
    pub fn sys(self, name: &str, value: impl Into<Value>) -> Self {
        self.with(Scope::Sys, name, value)
    }
}

impl Properties for MapProperties {
    fn get(&self, scope: Scope, name: &str) -> Option<Value> {
        self.map.get(&key(scope, name)).map(|(_, v)| v.clone())
    }
}

impl PropertiesMut for MapProperties {
    fn set(&mut self, scope: Scope, name: &str, value: Value) {
        self.map.insert(key(scope, name), (name.to_string(), value));
    }
    fn remove(&mut self, scope: Scope, name: &str) {
        self.map.remove(&key(scope, name));
    }
}
