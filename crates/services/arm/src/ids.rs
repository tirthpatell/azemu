//! Parsing of ARM resource paths.
//!
//! `/subscriptions/{sub}/resourceGroups/{rg}/providers/{ns}/{type}/{name}/{childType}/{childName}[/{action}]`

pub struct Scope {
    pub sub: Option<String>,
    pub rg: Option<String>,
    /// Segments after the subscription / resource group part.
    pub rest: Vec<String>,
}

/// Split a decoded path into subscription, resource group and the rest.
pub fn parse_scope(segs: &[String]) -> Scope {
    let mut sub = None;
    let mut rg = None;
    let mut idx = 0;
    if segs.len() >= 2 && segs[0].eq_ignore_ascii_case("subscriptions") {
        sub = Some(segs[1].clone());
        idx = 2;
        if segs.len() >= 4 && segs[2].eq_ignore_ascii_case("resourcegroups") {
            rg = Some(segs[3].clone());
            idx = 4;
        }
    }
    Scope { sub, rg, rest: segs[idx..].to_vec() }
}

/// What comes after `providers/{ns}/`: pairs of type and name, plus one trailing
/// segment when the count is odd (a collection to list, or an action to run).
#[derive(Debug, Clone, Default)]
pub struct ResPath {
    pub types: Vec<String>,
    pub names: Vec<String>,
    pub trailing: Option<String>,
}

pub fn parse_res(tail: &[String]) -> ResPath {
    let mut p = ResPath::default();
    let mut i = 0;
    while i + 1 < tail.len() {
        p.types.push(tail[i].clone());
        p.names.push(tail[i + 1].clone());
        i += 2;
    }
    if i < tail.len() {
        p.trailing = Some(tail[i].clone());
    }
    p
}

pub fn group_id(sub: &str, rg: &str) -> String {
    format!("/subscriptions/{sub}/resourceGroups/{rg}")
}

/// Canonical resource id from canonical pieces.
pub fn resource_id(sub: &str, rg: &str, ns: &str, types: &[String], names: &[String]) -> String {
    // A subscription-level resource has no group part.
    let scope = if rg.is_empty() { format!("/subscriptions/{sub}") } else { group_id(sub, rg) };
    let mut id = format!("{scope}/providers/{ns}");
    for (t, n) in types.iter().zip(names) {
        id.push('/');
        id.push_str(t);
        id.push('/');
        id.push_str(n);
    }
    id
}

pub fn type_string(ns: &str, types: &[String]) -> String {
    format!("{ns}/{}", types.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(p: &str) -> Vec<String> {
        p.split('/').filter(|s| !s.is_empty()).map(String::from).collect()
    }

    #[test]
    fn scope_and_resource() {
        let s = parse_scope(&segs("/subscriptions/s1/resourcegroups/rg1/providers/Microsoft.Storage/storageAccounts/a/listKeys"));
        assert_eq!(s.sub.as_deref(), Some("s1"));
        assert_eq!(s.rg.as_deref(), Some("rg1"));
        assert_eq!(s.rest[0], "providers");
        let r = parse_res(&s.rest[2..]);
        assert_eq!(r.types, vec!["storageAccounts"]);
        assert_eq!(r.names, vec!["a"]);
        assert_eq!(r.trailing.as_deref(), Some("listKeys"));
    }

    #[test]
    fn list_of_groups_has_no_rg() {
        let s = parse_scope(&segs("/subscriptions/s1/resourcegroups"));
        assert!(s.rg.is_none());
        assert_eq!(s.rest, vec!["resourcegroups"]);
    }

    #[test]
    fn double_slash_is_ignored() {
        let s = parse_scope(&segs("/subscriptions/s/resourcegroups/r/providers/Microsoft.Compute//availabilitySets/x"));
        let r = parse_res(&s.rest[2..]);
        assert_eq!(r.names, vec!["x"]);
    }
}
