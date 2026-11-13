//! The filter grammar of `key`, `label` and `name` parameters, `tags` filters and `$select`.
//!
//! A filter is a comma separated list; a value matches when any item matches:
//!
//! | item | matches |
//! |---|---|
//! | `abc` | exactly `abc` |
//! | `abc*` | starts with `abc` |
//! | `*abc` | ends with `abc` |
//! | `*abc*` | contains `abc` |
//! | `*` | everything (including the null label) |
//! | `\0` (or the NUL character, `%00` on the wire) | the null label (label filters only) |
//!
//! `\` escapes the next character, so `\*`, `\,` and `\\` are literal. A `*` anywhere
//! but the start or end of an item is a 400 at that position (`Invalid character`).

/// A filter that cannot be parsed. `pos` is the character position in the filter text.
#[derive(Debug, PartialEq, Eq)]
pub struct FilterError {
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Any,
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    /// The null label.
    Null,
}

impl Pattern {
    fn matches(&self, v: Option<&str>) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Null => v.is_none(),
            Pattern::Exact(s) => v == Some(s.as_str()),
            Pattern::Prefix(s) => v.map(|v| v.starts_with(s.as_str())).unwrap_or(false),
            Pattern::Suffix(s) => v.map(|v| v.ends_with(s.as_str())).unwrap_or(false),
            Pattern::Contains(s) => v.map(|v| v.contains(s.as_str())).unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    patterns: Vec<Pattern>,
}

#[derive(Default)]
struct Item {
    leading: bool,
    text: String,
    /// Position of an unescaped `*` after the text, until we know nothing follows it.
    trailing: Option<usize>,
}

impl Item {
    fn finish(self, label: bool) -> Pattern {
        let any_star = self.leading || self.trailing.is_some();
        if self.text.is_empty() && any_star {
            return Pattern::Any;
        }
        match (self.leading, self.trailing.is_some()) {
            (false, false) => {
                if label && self.text == "\0" {
                    Pattern::Null
                } else {
                    Pattern::Exact(self.text)
                }
            }
            (false, true) => Pattern::Prefix(self.text),
            (true, false) => Pattern::Suffix(self.text),
            (true, true) => Pattern::Contains(self.text),
        }
    }
}

impl Filter {
    /// Matches everything.
    pub fn any() -> Filter {
        Filter { patterns: Vec::new() }
    }

    /// Parse the text of a `key` / `label` / `name` parameter. `label` turns on `\0`.
    /// Empty text means no filter.
    pub fn parse(input: &str, label: bool) -> Result<Filter, FilterError> {
        if input.is_empty() {
            return Ok(Filter::any());
        }
        let chars: Vec<char> = input.chars().collect();
        let mut patterns = Vec::new();
        let mut cur = Item::default();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '\\' => {
                    let Some(&next) = chars.get(i + 1) else {
                        return Err(FilterError { pos: i });
                    };
                    if let Some(star) = cur.trailing {
                        return Err(FilterError { pos: star });
                    }
                    // `\0` is the null label (as a NUL character) in a label filter
                    cur.text.push(if label && next == '0' { '\0' } else { next });
                    i += 2;
                    continue;
                }
                ',' => {
                    patterns.push(std::mem::take(&mut cur).finish(label));
                }
                '*' => {
                    if cur.text.is_empty() && !cur.leading && cur.trailing.is_none() {
                        cur.leading = true;
                    } else if cur.trailing.is_some() {
                        return Err(FilterError { pos: cur.trailing.unwrap_or(i) });
                    } else {
                        cur.trailing = Some(i);
                    }
                }
                other => {
                    if let Some(star) = cur.trailing {
                        return Err(FilterError { pos: star });
                    }
                    cur.text.push(other);
                }
            }
            i += 1;
        }
        patterns.push(cur.finish(label));
        Ok(Filter { patterns })
    }

    /// Does `v` pass? Keys are always `Some`; the null label is `None`.
    /// How many comma-separated values the filter has (0 for "any").
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn matches(&self, v: Option<&str>) -> bool {
        self.patterns.is_empty() || self.patterns.iter().any(|p| p.matches(v))
    }

    pub fn matches_str(&self, v: &str) -> bool {
        self.matches(Some(v))
    }
}

/// Parse the `tags` filters (`tags=name=value`, repeated). All must match.
pub fn parse_tags<'a>(values: impl Iterator<Item = &'a str>) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for raw in values {
        let chars: Vec<char> = raw.chars().collect();
        let mut name = String::new();
        let mut value = String::new();
        let mut in_value = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' {
                let Some(&next) = chars.get(i + 1) else {
                    return Err(format!("tags({i}): Invalid character"));
                };
                if in_value {
                    value.push(next)
                } else {
                    name.push(next)
                }
                i += 2;
                continue;
            }
            if c == '=' && !in_value {
                in_value = true;
            } else if in_value {
                value.push(c);
            } else {
                name.push(c);
            }
            i += 1;
        }
        if !in_value || name.is_empty() {
            return Err(format!("tags: '{raw}' is not in the form name=value"));
        }
        out.push((name, value));
        if out.len() > crate::lists::MAX_FILTER_VALUES {
            return Err(format!("tags: at most {} tag filters are allowed.", crate::lists::MAX_FILTER_VALUES));
        }
    }
    Ok(out)
}

/// Do these tags contain every wanted `name=value` pair?
/// Does the item carry every wanted tag? One pass over the item's tags, each compared with the wanted ones (at most
/// `MAX_FILTER_VALUES` of them in a request), and it stops as soon as all are found. The cost is the number of tags of the
/// item, not tags times filters times items.
pub fn tags_match(tags: &[(String, String)], wanted: &[(String, String)]) -> bool {
    if wanted.is_empty() {
        return true;
    }
    let mut found = vec![false; wanted.len()];
    let mut missing = wanted.len();
    for (tn, tv) in tags {
        for (i, (n, v)) in wanted.iter().enumerate() {
            if !found[i] && tn == n && tv == v {
                found[i] = true;
                missing -= 1;
            }
        }
        if missing == 0 {
            return true;
        }
    }
    false
}

/// `$select`: comma separated field names, kept in the order of `allowed`.
/// `Ok(None)` when absent. `Err(name)` for a field that does not exist.
pub fn parse_select<'a>(values: impl Iterator<Item = &'a str>, allowed: &[&'static str]) -> Result<Option<Vec<&'static str>>, String> {
    let mut wanted: Vec<String> = Vec::new();
    let mut seen_any = false;
    for v in values {
        seen_any = true;
        for part in v.split(',') {
            let p = part.trim().to_ascii_lowercase();
            if !p.is_empty() {
                wanted.push(p);
            }
        }
    }
    if !seen_any || wanted.is_empty() {
        return Ok(None);
    }
    for w in &wanted {
        if !allowed.contains(&w.as_str()) {
            return Err(w.clone());
        }
    }
    Ok(Some(allowed.iter().copied().filter(|a| wanted.iter().any(|w| w == a)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(f: &str) -> Filter {
        Filter::parse(f, false).unwrap()
    }
    fn label(f: &str) -> Filter {
        Filter::parse(f, true).unwrap()
    }

    #[test]
    fn exact_prefix_suffix_contains_any() {
        assert!(key("abc").matches_str("abc"));
        assert!(!key("abc").matches_str("abcd"));
        assert!(key("abc*").matches_str("abcd"));
        assert!(key("abc*").matches_str("abc"));
        assert!(!key("abc*").matches_str("xabc"));
        assert!(key("*abc").matches_str("xxabc"));
        assert!(!key("*abc").matches_str("abcx"));
        assert!(key("*abc*").matches_str("xxabcxx"));
        assert!(!key("*abc*").matches_str("ab"));
        assert!(key("*").matches_str("anything"));
        assert!(key("**").matches_str("anything"));
        assert!(key("").matches_str("anything"));
    }

    #[test]
    fn keys_are_case_sensitive() {
        assert!(!key("abc").matches_str("ABC"));
        assert!(!key("ab*").matches_str("ABc"));
    }

    #[test]
    fn comma_lists_are_an_or() {
        let f = key("a,b*,*c");
        assert!(f.matches_str("a"));
        assert!(f.matches_str("bxx"));
        assert!(f.matches_str("xxc"));
        assert!(!f.matches_str("ax"));
    }

    #[test]
    fn escapes_make_reserved_characters_literal() {
        let f = key("a\\,b\\*c");
        assert!(f.matches_str("a,b*c"));
        assert!(!f.matches_str("a"));
        assert!(!f.matches_str("aXb*c"));
        let f = key("a\\\\b");
        assert!(f.matches_str("a\\b"));
        // an escaped star at the end is a literal, not a wildcard
        let f = key("a\\*");
        assert!(f.matches_str("a*"));
        assert!(!f.matches_str("ab"));
        // escaped star followed by a real wildcard
        let f = key("a\\**");
        assert!(f.matches_str("a*zzz"));
    }

    #[test]
    fn the_recorded_reserved_character_label() {
        let f = label("lbl_\\*\\, \\\\1");
        assert!(f.matches(Some("lbl_*, \\1")));
        assert!(!f.matches(Some("lbl_plain")));
    }

    #[test]
    fn null_label() {
        let f = label("\0");
        assert!(f.matches(None));
        assert!(!f.matches(Some("x")));
        let f = label("\\0");
        assert!(f.matches(None));
        let f = label("\0,prod");
        assert!(f.matches(None));
        assert!(f.matches(Some("prod")));
        assert!(!f.matches(Some("dev")));
        // a key filter has no null: a NUL is just a character
        assert!(!key("\0").matches(None));
    }

    #[test]
    fn star_matches_the_null_label_but_a_prefix_does_not() {
        assert!(label("*").matches(None));
        assert!(!label("dev*").matches(None));
        assert!(!label("dev").matches(None));
    }

    #[test]
    fn star_in_the_middle_is_invalid_at_its_position() {
        // the recorded 400: label(6): Invalid character
        assert_eq!(Filter::parse("test'@*$!%", true), Err(FilterError { pos: 6 }));
        assert_eq!(Filter::parse("a*b", false), Err(FilterError { pos: 1 }));
        assert_eq!(Filter::parse("*a*b*", false), Err(FilterError { pos: 2 }));
        assert_eq!(Filter::parse("x,a*b", false), Err(FilterError { pos: 3 }));
    }

    #[test]
    fn a_trailing_backslash_is_invalid() {
        assert_eq!(Filter::parse("abc\\", false), Err(FilterError { pos: 3 }));
    }

    #[test]
    fn other_special_characters_are_plain() {
        assert!(key("test'@$!%").matches_str("test'@$!%"));
    }

    #[test]
    fn tag_filters() {
        let wanted = parse_tags(["env=prod", "team=a"].into_iter()).unwrap();
        let tags = vec![("env".to_string(), "prod".to_string()), ("team".to_string(), "a".to_string())];
        assert!(tags_match(&tags, &wanted));
        assert!(!tags_match(&tags[..1], &wanted));
        assert!(parse_tags(["novalue"].into_iter()).is_err());
        assert!(parse_tags(["=v"].into_iter()).is_err());
        let w = parse_tags(["a\\=b=c=d"].into_iter()).unwrap();
        assert_eq!(w, vec![("a=b".to_string(), "c=d".to_string())]);
    }

    #[test]
    fn select_keeps_field_order_and_rejects_unknown_fields() {
        const F: &[&str] = &["etag", "key", "label", "value"];
        assert_eq!(parse_select(["value,key"].into_iter(), F), Ok(Some(vec!["key", "value"])));
        assert_eq!(parse_select(["KEY"].into_iter(), F), Ok(Some(vec!["key"])));
        assert_eq!(parse_select(std::iter::empty(), F), Ok(None));
        assert_eq!(parse_select(["key,bogus"].into_iter(), F), Err("bogus".to_string()));
    }
}
