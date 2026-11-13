//! Subscription rules: true / false / SQL / correlation filters and SQL actions, and how a
//! message is matched against all rules of a subscription.
//!
//! Copy semantics follow Microsoft's "Topic filters and actions": rules without an action are
//! combined with OR and yield one copy; every matching rule that has an action yields its own
//! copy, modified by that action.

use azemu_amqp::message::{Message, Properties as AmqpProps};
use azemu_amqp::types::Value as AmqpValue;
use azemu_servicebus_sql::{Action, Filter, Params, Properties, PropertiesMut, Scope, Value as SqlValue};

use crate::core::Incoming;

pub const DEFAULT_RULE_NAME: &str = "$Default";

/// The user properties of a correlation filter. A map in how it is used (`insert` replaces the value of a key that is there), but it
/// keeps the keys in the order they were first written, because that is the order Azure answers them in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropertyMap(Vec<(String, SqlValue)>);

impl PropertyMap {
    pub fn insert(&mut self, key: String, value: SqlValue) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &SqlValue)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn get(&self, key: &str) -> Option<&SqlValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

impl FromIterator<(String, SqlValue)> for PropertyMap {
    fn from_iter<I: IntoIterator<Item = (String, SqlValue)>>(iter: I) -> Self {
        let mut m = PropertyMap::default();
        m.extend(iter);
        m
    }
}

impl Extend<(String, SqlValue)> for PropertyMap {
    fn extend<I: IntoIterator<Item = (String, SqlValue)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<'a> IntoIterator for &'a PropertyMap {
    type Item = (&'a String, &'a SqlValue);
    type IntoIter = Box<dyn Iterator<Item = (&'a String, &'a SqlValue)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CorrelationFilter {
    pub correlation_id: Option<String>,
    pub message_id: Option<String>,
    pub to: Option<String>,
    pub reply_to: Option<String>,
    pub label: Option<String>,
    pub session_id: Option<String>,
    pub reply_to_session_id: Option<String>,
    pub content_type: Option<String>,
    /// In the order the client wrote them (Azure answers them in that order; recording
    /// `mgmt_tests/test_mgmt_rules.pyTestServiceBusAdministrationClientRuletest_mgmt_rule_create.json`, entry 3 against entry 4).
    pub properties: PropertyMap,
}

#[derive(Debug, Clone)]
pub enum RuleFilter {
    True,
    False,
    Sql { expression: String, parameters: Params, compatibility_level: i32, compiled: Filter },
    Correlation(CorrelationFilter),
}

#[derive(Debug, Clone)]
pub struct RuleAction {
    pub expression: String,
    pub parameters: Params,
    pub compiled: Action,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub filter: RuleFilter,
    pub action: Option<RuleAction>,
    pub created_ms: i64,
}

impl Rule {
    pub fn default_rule(now: i64) -> Rule {
        Rule {
            name: DEFAULT_RULE_NAME.to_string(),
            filter: RuleFilter::Sql {
                expression: "1=1".into(),
                parameters: Params::new(),
                compatibility_level: 20,
                compiled: Filter::parse("1=1").expect("1=1 parses"),
            },
            action: None,
            created_ms: now,
        }
    }

    pub fn sql_filter(name: &str, expression: &str, parameters: Params, now: i64) -> Result<Rule, String> {
        let compiled = Filter::parse(expression).map_err(|e| e.to_string())?;
        Ok(Rule {
            name: name.to_string(),
            filter: RuleFilter::Sql { expression: expression.to_string(), parameters, compatibility_level: 20, compiled },
            action: None,
            created_ms: now,
        })
    }

    pub fn with_action(mut self, expression: &str, parameters: Params) -> Result<Rule, String> {
        if expression.trim().is_empty() {
            self.action = None;
            return Ok(self);
        }
        let compiled = Action::parse(expression).map_err(|e| e.to_string())?;
        self.action = Some(RuleAction { expression: expression.to_string(), parameters, compiled });
        Ok(self)
    }

    fn matches(&self, msg: &Message) -> bool {
        match &self.filter {
            RuleFilter::True => true,
            RuleFilter::False => false,
            RuleFilter::Sql { compiled, parameters, .. } => compiled.try_matches(&MsgProps(msg), parameters).unwrap_or(false),
            RuleFilter::Correlation(c) => correlation_matches(c, msg),
        }
    }
}

/// The messages to enqueue in a subscription for `inc`: empty when no rule matches.
pub fn evaluate(rules: &[Rule], inc: &Incoming, _dead_letter_on_exception: bool) -> Vec<Message> {
    let mut plain = false;
    let mut with_action = Vec::new();
    for rule in rules {
        if !rule.matches(&inc.message) {
            continue;
        }
        match &rule.action {
            None => plain = true,
            Some(a) => {
                let mut copy = inc.message.clone();
                if a.compiled.try_apply(&mut MsgPropsMut(&mut copy), &a.parameters).is_ok() {
                    crate::core::set_prop(
                        copy.application_properties.get_or_insert_with(Vec::new),
                        "RuleName",
                        AmqpValue::String(rule.name.clone()),
                    );
                    with_action.push(copy);
                }
            }
        }
    }
    let mut out = Vec::new();
    if plain {
        out.push(inc.message.clone());
    }
    out.extend(with_action);
    out
}

fn eq_opt(want: &Option<String>, have: Option<&str>) -> bool {
    match want {
        None => true,
        Some(w) => have == Some(w.as_str()),
    }
}

fn id_string(v: &Option<AmqpValue>) -> Option<String> {
    crate::core::message_id_string(&Message {
        properties: Some(AmqpProps { message_id: v.clone(), ..Default::default() }),
        ..Default::default()
    })
}

fn correlation_matches(c: &CorrelationFilter, m: &Message) -> bool {
    let empty = AmqpProps::default();
    let p = m.properties.as_ref().unwrap_or(&empty);
    let cid = id_string(&p.correlation_id);
    let mid = id_string(&p.message_id);
    if !eq_opt(&c.correlation_id, cid.as_deref())
        || !eq_opt(&c.message_id, mid.as_deref())
        || !eq_opt(&c.to, p.to.as_deref())
        || !eq_opt(&c.reply_to, p.reply_to.as_deref())
        || !eq_opt(&c.label, p.subject.as_deref())
        || !eq_opt(&c.session_id, p.group_id.as_deref())
        || !eq_opt(&c.reply_to_session_id, p.reply_to_group_id.as_deref())
        || !eq_opt(&c.content_type, p.content_type.as_deref())
    {
        return false;
    }
    let props = MsgProps(m);
    c.properties.iter().all(|(k, want)| props.get(Scope::User, k).as_ref() == Some(want))
}

fn to_sql(v: &AmqpValue) -> Option<SqlValue> {
    Some(match v {
        AmqpValue::Null => SqlValue::Null,
        AmqpValue::Bool(b) => SqlValue::Bool(*b),
        AmqpValue::String(s) | AmqpValue::Symbol(s) => SqlValue::Str(s.clone()),
        AmqpValue::Float(f) => SqlValue::Double(*f as f64),
        AmqpValue::Double(f) => SqlValue::Double(*f),
        AmqpValue::Timestamp(t) => SqlValue::Timestamp(*t),
        AmqpValue::Described(d) => return to_sql(&d.value),
        other => SqlValue::Long(other.as_i64()?),
    })
}

fn from_sql(v: &SqlValue) -> AmqpValue {
    match v {
        SqlValue::Null => AmqpValue::Null,
        SqlValue::Bool(b) => AmqpValue::Bool(*b),
        SqlValue::Long(l) => AmqpValue::Long(*l),
        SqlValue::Double(d) => AmqpValue::Double(*d),
        SqlValue::Str(s) => AmqpValue::String(s.clone()),
        SqlValue::Timestamp(t) => AmqpValue::Timestamp(*t),
    }
}

/// Read access to a message for the SQL language: `sys.` names are the standard broker
/// properties, everything else is an application property.
pub struct MsgProps<'a>(pub &'a Message);

impl Properties for MsgProps<'_> {
    fn get(&self, scope: Scope, name: &str) -> Option<SqlValue> {
        let m = self.0;
        match scope {
            Scope::User => {
                let props = m.application_properties.as_ref()?;
                props.iter().find(|(k, _)| k.as_str().map(|k| k.eq_ignore_ascii_case(name)).unwrap_or(false)).and_then(|(_, v)| to_sql(v))
            }
            Scope::Sys => {
                let p = m.properties.as_ref();
                let lower = name.to_ascii_lowercase();
                let s = |v: Option<&String>| v.map(|s| SqlValue::Str(s.clone()));
                match lower.as_str() {
                    "messageid" => id_string(&p?.message_id).map(SqlValue::Str),
                    "correlationid" => id_string(&p?.correlation_id).map(SqlValue::Str),
                    "to" => s(p?.to.as_ref()),
                    "replyto" => s(p?.reply_to.as_ref()),
                    "label" | "subject" => s(p?.subject.as_ref()),
                    "sessionid" => s(p?.group_id.as_ref()),
                    "replytosessionid" => s(p?.reply_to_group_id.as_ref()),
                    "contenttype" => s(p?.content_type.as_ref()),
                    "partitionkey" => m.annotation(crate::core::ANN_PARTITION_KEY).and_then(to_sql),
                    "timetolive" => m.header.as_ref().and_then(|h| h.ttl).map(|t| SqlValue::Long(t as i64)),
                    "deliverycount" => Some(SqlValue::Long(m.header.as_ref().and_then(|h| h.delivery_count).unwrap_or(0) as i64)),
                    _ => None,
                }
            }
        }
    }
}

pub struct MsgPropsMut<'a>(pub &'a mut Message);

impl Properties for MsgPropsMut<'_> {
    fn get(&self, scope: Scope, name: &str) -> Option<SqlValue> {
        MsgProps(self.0).get(scope, name)
    }
}

impl PropertiesMut for MsgPropsMut<'_> {
    fn set(&mut self, scope: Scope, name: &str, value: SqlValue) {
        let m = &mut *self.0;
        match scope {
            Scope::User => {
                let props = m.application_properties.get_or_insert_with(Vec::new);
                let v = from_sql(&value);
                match props.iter_mut().find(|(k, _)| k.as_str().map(|k| k.eq_ignore_ascii_case(name)).unwrap_or(false)) {
                    Some(slot) => slot.1 = v,
                    None => props.push((AmqpValue::String(name.to_string()), v)),
                }
            }
            Scope::Sys => {
                let p = m.properties.get_or_insert_with(AmqpProps::default);
                let text = match &value {
                    SqlValue::Str(s) => Some(s.clone()),
                    SqlValue::Null => None,
                    other => Some(match other {
                        SqlValue::Bool(b) => b.to_string(),
                        SqlValue::Long(l) => l.to_string(),
                        SqlValue::Double(d) => d.to_string(),
                        SqlValue::Timestamp(t) => t.to_string(),
                        _ => String::new(),
                    }),
                };
                match name.to_ascii_lowercase().as_str() {
                    "messageid" => p.message_id = text.map(AmqpValue::String),
                    "correlationid" => p.correlation_id = text.map(AmqpValue::String),
                    "to" => p.to = text,
                    "replyto" => p.reply_to = text,
                    "label" | "subject" => p.subject = text,
                    "sessionid" => p.group_id = text,
                    "replytosessionid" => p.reply_to_group_id = text,
                    "contenttype" => p.content_type = text,
                    "partitionkey" => {
                        if let Some(t) = text {
                            m.set_annotation(crate::core::ANN_PARTITION_KEY, AmqpValue::String(t));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn remove(&mut self, scope: Scope, name: &str) {
        if scope == Scope::User {
            if let Some(props) = self.0.application_properties.as_mut() {
                props.retain(|(k, _)| !k.as_str().map(|k| k.eq_ignore_ascii_case(name)).unwrap_or(false));
            }
        }
    }
}
