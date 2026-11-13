#![allow(dead_code)]
pub use azemu_servicebus_sql::*;

pub fn props() -> MapProperties {
    MapProperties::new()
}

/// Parse (panicking with the filter text if it does not parse) and match.
pub fn m(sql: &str, p: &MapProperties) -> bool {
    m_with(sql, p, &Params::new())
}

pub fn m_with(sql: &str, p: &MapProperties, params: &Params) -> bool {
    match Filter::parse(sql) {
        Ok(f) => f.matches(p, params),
        Err(e) => panic!("filter {sql:?} should parse: {e}"),
    }
}

pub fn parse_err(sql: &str) -> ParseError {
    match Filter::parse(sql) {
        Ok(_) => panic!("filter {sql:?} should not parse"),
        Err(e) => e,
    }
}

pub fn act(sql: &str, p: &mut MapProperties) {
    act_with(sql, p, &Params::new());
}

pub fn act_with(sql: &str, p: &mut MapProperties, params: &Params) {
    match Action::parse(sql) {
        Ok(a) => a.try_apply(p, params).unwrap_or_else(|e| panic!("{sql:?} failed: {e}")),
        Err(e) => panic!("action {sql:?} should parse: {e}"),
    }
}

pub fn params(items: &[(&str, Value)]) -> Params {
    items.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

/// `t!(name, sql, props, expected)` generates one test.
#[macro_export]
macro_rules! t {
    ($name:ident, $sql:expr, $props:expr, $exp:expr) => {
        #[test]
        fn $name() {
            let p: MapProperties = $props;
            assert_eq!($crate::common::m($sql, &p), $exp, "filter: {}", $sql);
        }
    };
}
