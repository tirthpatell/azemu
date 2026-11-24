//! `*` matching as Azure RBAC does it for action strings: case-insensitive, and a `*` stands for any run of
//! characters, a `/` included (`Microsoft.Storage/*/read` matches `Microsoft.Storage/storageAccounts/blobServices/containers/read`).

/// Does `text` match `pattern`? ASCII case is ignored; every `*` matches any (possibly empty) run of characters.
pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Is `action` allowed by `allow` and not taken away by `deny`? (One role definition's rule: the `not` list removes from
/// that role's own list only.)
pub fn permitted(allow: &[String], deny: &[String], action: &str) -> bool {
    allow.iter().any(|a| matches(a, action)) && !deny.iter().any(|d| matches(d, action))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_across_slashes_and_any_case() {
        assert!(matches("*", "Microsoft.Storage/x/y/read"));
        assert!(matches("Microsoft.Storage/*/read", "microsoft.storage/storageAccounts/blobServices/containers/READ"));
        assert!(matches("Microsoft.KeyVault/vaults/*", "Microsoft.KeyVault/vaults/secrets/getSecret/action"));
        assert!(matches("Microsoft.ServiceBus/*/send/action", "Microsoft.ServiceBus/namespaces/queues/send/action"));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(matches("a*", "a"));
    }

    #[test]
    fn without_a_star_the_whole_text_must_match() {
        assert!(!matches("Microsoft.Storage/x/read", "Microsoft.Storage/x/read/more"));
        assert!(!matches("Microsoft.Storage/x/read", "Microsoft.Storage/x/rea"));
        assert!(!matches("a*b", "a"));
        assert!(!matches("*/read", "x/write"));
        assert!(!matches("", "x"));
        assert!(matches("", ""));
    }

    #[test]
    fn a_not_list_removes_from_the_same_role_only() {
        let allow = vec!["Microsoft.Storage/*".to_string()];
        let deny = vec!["Microsoft.Storage/x/delete".to_string()];
        assert!(permitted(&allow, &deny, "Microsoft.Storage/x/read"));
        assert!(!permitted(&allow, &deny, "Microsoft.Storage/x/delete"));
        assert!(!permitted(&[], &[], "anything"));
    }
}
