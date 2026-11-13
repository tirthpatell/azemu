//! Stored access policies (`?comp=acl`): parse, validate, render.

use azemu_core::time;

use crate::errors::{invalid_xml, Res};
use crate::state::{Policy, SignedId};
use crate::xml;

/// Parse a `Set Table ACL` body. At most 5 identifiers, ids unique and at most
/// 64 characters, permissions drawn from `r a u d`.
pub fn parse(body: &str) -> Res<Vec<SignedId>> {
    let root = xml::parse(body).map_err(|_| invalid_xml())?;
    if root.name != "SignedIdentifiers" {
        return Err(invalid_xml());
    }
    let mut out: Vec<SignedId> = Vec::new();
    for n in root.children_named("SignedIdentifier") {
        let id = n.child_text("Id").ok_or_else(invalid_xml)?.to_string();
        if id.is_empty() || id.chars().count() > 64 || out.iter().any(|s| s.id == id) {
            return Err(invalid_xml());
        }
        let policy = match n.child("AccessPolicy") {
            Some(p) if !p.children.is_empty() => {
                let when = |name: &str| -> Res<Option<String>> {
                    match p.child_text(name) {
                        None => Ok(None),
                        Some(t) => {
                            let parsed = time::parse_iso8601(t).ok_or_else(invalid_xml)?;
                            Ok(Some(time::iso8601_7(parsed)))
                        }
                    }
                };
                let permission = p.child_text("Permission").map(str::to_string);
                if let Some(perm) = &permission {
                    if !perm.chars().all(|c| matches!(c, 'r' | 'a' | 'u' | 'd')) {
                        return Err(invalid_xml());
                    }
                }
                Some(Policy { start: when("Start")?, expiry: when("Expiry")?, permission })
            }
            _ => None,
        };
        out.push(SignedId { id, policy });
    }
    if out.len() > 5 {
        return Err(invalid_xml());
    }
    Ok(out)
}

pub fn render(ids: &[SignedId]) -> String {
    let head = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>";
    if ids.is_empty() {
        return format!("{head}<SignedIdentifiers />");
    }
    let mut s = format!("{head}<SignedIdentifiers>");
    for i in ids {
        s.push_str(&format!("<SignedIdentifier><Id>{}</Id>", xml::escape(&i.id)));
        if let Some(p) = &i.policy {
            s.push_str("<AccessPolicy>");
            if let Some(v) = &p.start {
                s.push_str(&format!("<Start>{v}</Start>"));
            }
            if let Some(v) = &p.expiry {
                s.push_str(&format!("<Expiry>{v}</Expiry>"));
            }
            if let Some(v) = &p.permission {
                s.push_str(&format!("<Permission>{}</Permission>", xml::escape(v)));
            }
            s.push_str("</AccessPolicy>");
        }
        s.push_str("</SignedIdentifier>");
    }
    s.push_str("</SignedIdentifiers>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_normalises_times() {
        let ids = parse("<?xml version='1.0' encoding='utf-8'?>\n<SignedIdentifiers><SignedIdentifier><Id>testid</Id><AccessPolicy><Start>2021-06-08T02:05:09Z</Start><Expiry>2021-06-08T03:10:09Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>").unwrap();
        assert_eq!(
            render(&ids),
            "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><SignedIdentifiers><SignedIdentifier><Id>testid</Id><AccessPolicy><Start>2021-06-08T02:05:09.0000000Z</Start><Expiry>2021-06-08T03:10:09.0000000Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>"
        );
    }

    #[test]
    fn empty_policy_is_dropped_partial_is_kept() {
        let ids = parse("<SignedIdentifiers><SignedIdentifier><Id>null</Id></SignedIdentifier><SignedIdentifier><Id>empty</Id><AccessPolicy /></SignedIdentifier><SignedIdentifier><Id>partial</Id><AccessPolicy><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>").unwrap();
        assert_eq!(ids[1].policy, None);
        assert_eq!(ids[2].policy.as_ref().unwrap().permission.as_deref(), Some("r"));
        assert!(render(&ids).contains("<Id>empty</Id></SignedIdentifier>"));
    }

    #[test]
    fn empty_list_renders_self_closing() {
        assert!(render(&[]).ends_with("<SignedIdentifiers />"));
    }

    #[test]
    fn limits_and_bad_input() {
        let many: String = (0..6).map(|i| format!("<SignedIdentifier><Id>id{i}</Id></SignedIdentifier>")).collect();
        assert_eq!(parse(&format!("<SignedIdentifiers>{many}</SignedIdentifiers>")).unwrap_err().code, "InvalidXmlDocument");
        assert!(parse("<SignedIdentifiers><SignedIdentifier><Id>a</Id></SignedIdentifier><SignedIdentifier><Id>a</Id></SignedIdentifier></SignedIdentifiers>").is_err());
        assert!(parse("<SignedIdentifiers><SignedIdentifier><Id>a</Id><AccessPolicy><Permission>z</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>").is_err());
        assert!(parse("<SignedIdentifiers><SignedIdentifier><Id>a</Id><AccessPolicy><Start>nope</Start></AccessPolicy></SignedIdentifier></SignedIdentifiers>").is_err());
        assert!(parse("<Wrong/>").is_err());
    }
}
