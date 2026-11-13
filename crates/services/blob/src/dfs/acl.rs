//! POSIX permissions and ACL strings of the Data Lake Gen2 API.
//!
//! An ACL is a list of [`Entry`]: `user::rwx`, `user:<id>:r-x`, `group::r-x`, `mask::rwx`,
//! `other::---` and the same with a `default:` prefix. Permissions (`rwxr-x---`, `0750`,
//! sticky bit) are a view of the access entries: user bits are `user::`, group bits are the
//! `mask::` entry when there is one (else `group::`), other bits are `other::`.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    User,
    Group,
    Mask,
    Other,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Group => "group",
            Kind::Mask => "mask",
            Kind::Other => "other",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub default: bool,
    pub kind: Kind,
    /// Object id of a named user or group; empty for the owner, the owning group, mask, other.
    pub id: String,
    /// rwx bits, 0..=7.
    pub perm: u8,
}

impl Entry {
    fn same_key(&self, o: &Entry) -> bool {
        self.default == o.default && self.kind == o.kind && self.id == o.id
    }
}

/// `rwx`-style or one octal digit.
pub fn parse_perm(s: &str) -> Option<u8> {
    let b = s.as_bytes();
    if b.len() == 1 && (b'0'..=b'7').contains(&b[0]) {
        return Some(b[0] - b'0');
    }
    if b.len() != 3 {
        return None;
    }
    let mut v = 0u8;
    for (i, (c, on)) in b.iter().zip(*b"rwx").enumerate() {
        match *c {
            x if x == on => v |= 4 >> i,
            b'-' => {}
            _ => return None,
        }
    }
    Some(v)
}

pub fn perm_str(p: u8) -> String {
    let mut s = String::with_capacity(3);
    s.push(if p & 4 != 0 { 'r' } else { '-' });
    s.push(if p & 2 != 0 { 'w' } else { '-' });
    s.push(if p & 1 != 0 { 'x' } else { '-' });
    s
}

fn parse_kind(s: &str) -> Option<Kind> {
    match s {
        "user" => Some(Kind::User),
        "group" => Some(Kind::Group),
        "mask" => Some(Kind::Mask),
        "other" => Some(Kind::Other),
        _ => None,
    }
}

/// One entry: `[default:]type:id:perm`. With `with_perm` false (remove mode) the permission
/// is absent: `[default:]type[:id]`.
fn parse_entry(s: &str, with_perm: bool) -> Result<Entry, String> {
    let s = s.trim();
    let (default, rest) = match s.strip_prefix("default:") {
        Some(r) => (true, r),
        None => (false, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let bad = || format!("The ACL entry '{s}' is not valid.");
    let kind = parse_kind(parts.first().copied().unwrap_or("")).ok_or_else(bad)?;
    if with_perm {
        if parts.len() != 3 {
            return Err(bad());
        }
        let perm = parse_perm(parts[2]).ok_or_else(bad)?;
        let id = parts[1];
        if matches!(kind, Kind::Mask | Kind::Other) && !id.is_empty() {
            return Err(bad());
        }
        Ok(Entry { default, kind, id: id.to_string(), perm })
    } else {
        if parts.len() > 2 {
            return Err(bad());
        }
        let id = parts.get(1).copied().unwrap_or("");
        Ok(Entry { default, kind, id: id.to_string(), perm: 0 })
    }
}

/// Azure's limit: an access ACL and a default ACL each hold at most 32 entries (the four base entries, the mask and the
/// named users and groups all count; "effectively 28" named entries). Source: "What are the limits for Azure role
/// assignments and ACL entries?" in the Data Lake Storage access control article.
pub const MAX_ENTRIES: usize = 32;

fn too_many(kind: &str) -> String {
    format!("The ACL has more than {MAX_ENTRIES} {kind} entries, the most a path can have.")
}

/// Refuse a list that has more than 32 access entries or more than 32 default entries.
pub fn check_limits(v: &[Entry]) -> Result<(), String> {
    let defaults = v.iter().filter(|e| e.default).count();
    if v.len() - defaults > MAX_ENTRIES {
        return Err(too_many("access"));
    }
    if defaults > MAX_ENTRIES {
        return Err(too_many("default"));
    }
    Ok(())
}

fn parse_list(s: &str, with_perm: bool) -> Result<Vec<Entry>, String> {
    let mut out = vec![];
    for part in s.split(',').filter(|p| !p.trim().is_empty()) {
        // stop early: a list this long is refused anyway, and nothing below should work on thousands of entries
        if out.len() >= 2 * MAX_ENTRIES {
            return Err(too_many("access or default"));
        }
        out.push(parse_entry(part, with_perm)?);
    }
    if out.is_empty() {
        return Err("The ACL is empty.".into());
    }
    check_limits(&out)?;
    Ok(out)
}

/// `x-ms-acl` as set / modify content.
pub fn parse_acl(s: &str) -> Result<Vec<Entry>, String> {
    parse_list(s, true)
}

/// `x-ms-acl` of a remove-mode request: entries without permissions.
pub fn parse_acl_remove(s: &str) -> Result<Vec<Entry>, String> {
    parse_list(s, false)
}

fn order_key(e: &Entry) -> (bool, u8) {
    let slot = match (e.kind, e.id.is_empty()) {
        (Kind::User, true) => 0,
        (Kind::User, false) => 1,
        (Kind::Group, true) => 2,
        (Kind::Group, false) => 3,
        (Kind::Mask, _) => 4,
        (Kind::Other, _) => 5,
    };
    (e.default, slot)
}

fn find(v: &[Entry], default: bool, kind: Kind) -> Option<&Entry> {
    v.iter().find(|e| e.default == default && e.kind == kind && e.id.is_empty())
}

/// Bring an entry list to the shape the service stores: no duplicates, the three base
/// entries present (taken from `fallback` or empty when missing), a mask when there are named
/// entries, a complete default set when any default entry exists, canonical order.
pub fn normalize(entries: Vec<Entry>, fallback: &[Entry]) -> Vec<Entry> {
    normalize_with(entries, fallback, true)
}

fn normalize_with(entries: Vec<Entry>, fallback: &[Entry], complete_defaults: bool) -> Vec<Entry> {
    let mut v: Vec<Entry> = vec![];
    for e in entries {
        if let Some(pos) = v.iter().position(|x| x.same_key(&e)) {
            v.remove(pos);
        }
        v.push(e);
    }
    for default in [false, true] {
        let any = v.iter().any(|e| e.default == default);
        if default && (!any || !complete_defaults) {
            continue;
        }
        for kind in [Kind::User, Kind::Group, Kind::Other] {
            if find(&v, default, kind).is_none() {
                let from = find(fallback, default, kind)
                    .or_else(|| find(&v, false, kind))
                    .or_else(|| find(fallback, false, kind))
                    .map(|e| e.perm)
                    .unwrap_or(0);
                v.push(Entry { default, kind, id: String::new(), perm: from });
            }
        }
        let named: u8 = v.iter().filter(|e| e.default == default && !e.id.is_empty()).fold(0, |acc, e| acc | e.perm);
        let has_named = v.iter().any(|e| e.default == default && !e.id.is_empty());
        if has_named && find(&v, default, Kind::Mask).is_none() {
            let group = find(&v, default, Kind::Group).map(|e| e.perm).unwrap_or(0);
            v.push(Entry { default, kind: Kind::Mask, id: String::new(), perm: named | group });
        }
    }
    v.sort_by_key(order_key);
    v
}

pub fn format_acl(v: &[Entry]) -> String {
    v.iter()
        .map(|e| format!("{}{}:{}:{}", if e.default { "default:" } else { "" }, e.kind.word(), e.id, perm_str(e.perm)))
        .collect::<Vec<_>>()
        .join(",")
}

/// The nine permission bits (`0o750`) of the access entries.
pub fn permission_bits(v: &[Entry]) -> u16 {
    let g = |kind| find(v, false, kind).map(|e| e.perm as u16).unwrap_or(0);
    let group = find(v, false, Kind::Mask).map(|e| e.perm as u16).unwrap_or_else(|| g(Kind::Group));
    (g(Kind::User) << 6) | (group << 3) | g(Kind::Other)
}

/// Apply permission bits to the access entries (group bits go to the mask when there is one).
pub fn set_permission_bits(v: &mut Vec<Entry>, bits: u16) {
    let (u, g, o) = (((bits >> 6) & 7) as u8, ((bits >> 3) & 7) as u8, (bits & 7) as u8);
    let has_mask = find(v, false, Kind::Mask).is_some();
    for e in v.iter_mut().filter(|e| !e.default && e.id.is_empty()) {
        match e.kind {
            Kind::User => e.perm = u,
            Kind::Other => e.perm = o,
            Kind::Mask => e.perm = g,
            Kind::Group if !has_mask => e.perm = g,
            Kind::Group => {}
        }
    }
    for (kind, perm) in [(Kind::User, u), (Kind::Group, g), (Kind::Other, o)] {
        if find(v, false, kind).is_none() {
            v.push(Entry { default: false, kind, id: String::new(), perm });
        }
    }
    v.sort_by_key(order_key);
}

/// The base entries for plain permission bits.
pub fn from_bits(bits: u16) -> Vec<Entry> {
    let mut v = vec![];
    set_permission_bits(&mut v, bits);
    v
}

/// `rwxr-x---`, sticky bit as `t` / `T`.
pub fn permissions_string(bits: u16, sticky: bool) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        s.push_str(&perm_str(((bits >> shift) & 7) as u8));
    }
    if sticky {
        let last = s.pop().unwrap_or('-');
        s.push(if last == 'x' { 't' } else { 'T' });
    }
    s
}

/// `x-ms-permissions` (symbolic `rwxr-x---` / `rwxr-xr-t`, or octal `0750` / `1777`) into
/// (nine bits, sticky).
pub fn parse_permissions(s: &str) -> Option<(u16, bool)> {
    let s = s.trim();
    if s.len() == 9 && !s.chars().all(|c| c.is_ascii_digit()) {
        let b = s.as_bytes();
        let mut bits = 0u16;
        let mut sticky = false;
        for (i, c) in b.iter().enumerate() {
            let on = b"rwx"[i % 3];
            let bit = 1u16 << (8 - i);
            match *c {
                x if x == on => bits |= bit,
                b'-' => {}
                b't' | b'T' if i == 8 => {
                    sticky = true;
                    if *c == b't' {
                        bits |= bit;
                    }
                }
                _ => return None,
            }
        }
        return Some((bits, sticky));
    }
    if (3..=4).contains(&s.len()) && s.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
        let v = u16::from_str_radix(s, 8).ok()?;
        return Some((v & 0o777, v & 0o1000 != 0));
    }
    None
}

/// `x-ms-umask` (octal, e.g. `0027`).
pub fn parse_umask(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
        return None;
    }
    u16::from_str_radix(s, 8).ok().map(|v| v & 0o777)
}

/// The access ACL a new child gets from the default entries of its parent; `None` when the
/// parent has no default entries. For a directory the defaults are copied as well.
pub fn inherit(parent: &[Entry], child_is_dir: bool) -> Option<Vec<Entry>> {
    if !parent.iter().any(|e| e.default) {
        return None;
    }
    let mut out: Vec<Entry> = vec![];
    for e in parent.iter().filter(|e| e.default) {
        out.push(Entry { default: false, ..e.clone() });
        if child_is_dir {
            out.push(e.clone());
        }
    }
    Some(normalize(out, &[]))
}

/// Set mode: the given entries replace everything (a file takes no default entries).
pub fn apply_set(current: &[Entry], given: &[Entry], is_dir: bool) -> Vec<Entry> {
    let given: Vec<Entry> = given.iter().filter(|e| is_dir || !e.default).cloned().collect();
    normalize(given, current)
}

/// Modify mode: given entries are added or replaced, the others stay.
pub fn apply_modify(current: &[Entry], given: &[Entry], is_dir: bool) -> Vec<Entry> {
    let mut v = current.to_vec();
    for g in given.iter().filter(|e| is_dir || !e.default) {
        match v.iter_mut().find(|e| e.same_key(g)) {
            Some(e) => e.perm = g.perm,
            None => v.push(g.clone()),
        }
    }
    normalize(v, current)
}

/// Remove mode: the named entries, the mask and the default entries listed are dropped. The
/// three access base entries cannot be removed.
pub fn apply_remove(current: &[Entry], given: &[Entry]) -> Vec<Entry> {
    let mut v = current.to_vec();
    v.retain(|e| {
        let listed = given.iter().any(|g| g.same_key(e));
        let base = !e.default && e.id.is_empty() && e.kind != Kind::Mask;
        !listed || base
    });
    // The default entries are not completed again: removing `default:user` takes it away.
    normalize_with(v, current, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn many(n: usize, default: bool) -> String {
        (0..n).map(|i| format!("{}user:{i:032x}:rwx", if default { "default:" } else { "" })).collect::<Vec<_>>().join(",")
    }

    #[test]
    fn at_most_32_access_and_32_default_entries() {
        // 28 named users and the four base entries (mask included) are the most an access ACL can hold
        let ok = format!("user::rwx,group::r-x,other::---,{}", many(29, false));
        assert!(parse_acl(&ok).is_ok(), "32 entries");
        let too_many = format!("user::rwx,group::r-x,other::---,{}", many(30, false));
        assert!(parse_acl(&too_many).unwrap_err().contains("32"));
        // access and default are counted apart
        let both = format!("user::rwx,group::r-x,other::---,{},{}", many(29, false), many(32, true));
        assert!(parse_acl(&both).is_ok());
        assert!(parse_acl(&many(33, true)).is_err());
        // a huge list is refused without being parsed to the end
        let huge = many(100_000, true);
        assert!(parse_acl(&huge).is_err() && parse_acl_remove(&huge).is_err());
        // after normalization too: a mask is added when there are named entries
        let v = normalize(parse_acl(&format!("user::rwx,group::r-x,other::---,{}", many(28, false))).unwrap(), &[]);
        assert_eq!(v.len(), 32);
        assert!(check_limits(&v).is_ok());
        let w = apply_modify(&v, &parse_acl("user:ffffffffffffffffffffffffffffffff:r--").unwrap(), true);
        assert!(check_limits(&w).is_err());
    }

    #[test]
    fn permissions_round_trip() {
        assert_eq!(parse_permissions("rwxr-x---"), Some((0o750, false)));
        assert_eq!(parse_permissions("0777"), Some((0o777, false)));
        assert_eq!(parse_permissions("1777"), Some((0o777, true)));
        assert_eq!(parse_permissions("rwxrwxrwt"), Some((0o777, true)));
        assert_eq!(parse_permissions("rwxrwxrwT"), Some((0o776, true)));
        assert_eq!(parse_permissions("rwx"), None);
        assert_eq!(permissions_string(0o750, false), "rwxr-x---");
        assert_eq!(permissions_string(0o777, true), "rwxrwxrwt");
        assert_eq!(permissions_string(0o770, true), "rwxrwx--T");
    }

    #[test]
    fn acl_parse_format_and_mask() {
        let v = normalize(parse_acl("user::rwx,group::r-x,other::---,user:abc:rw-").unwrap(), &[]);
        assert_eq!(format_acl(&v), "user::rwx,user:abc:rw-,group::r-x,mask::rwx,other::---");
        assert_eq!(permission_bits(&v), 0o770);
        let plain = from_bits(0o640);
        assert_eq!(format_acl(&plain), "user::rw-,group::r--,other::---");
        assert!(parse_acl("user::rwz").is_err());
        assert!(parse_acl("bogus::rwx").is_err());
        assert_eq!(parse_perm("5"), Some(5));
    }

    #[test]
    fn defaults_are_inherited() {
        let parent = normalize(
            parse_acl("user::rwx,group::r-x,other::---,default:user::rwx,default:group::r-x,default:other::r--,default:user:abc:rwx")
                .unwrap(),
            &[],
        );
        let dir = inherit(&parent, true).unwrap();
        let s = format_acl(&dir);
        assert!(s.contains("user:abc:rwx"), "{s}");
        assert!(s.contains("default:user:abc:rwx"), "{s}");
        let file = inherit(&parent, false).unwrap();
        assert!(!format_acl(&file).contains("default:"));
        assert!(inherit(&from_bits(0o750), true).is_none());
    }

    #[test]
    fn modify_and_remove() {
        let cur = normalize(parse_acl("user::rwx,group::r-x,other::---").unwrap(), &[]);
        let m = apply_modify(&cur, &parse_acl("user:abc:r--,default:user::rwx").unwrap(), true);
        let s = format_acl(&m);
        assert!(s.contains("user:abc:r--") && s.contains("mask::r-x") && s.contains("default:user::rwx"), "{s}");
        let r = apply_remove(&m, &parse_acl_remove("mask,user:abc,default:user").unwrap());
        // the mask is calculated again only when named entries exist
        assert!(!format_acl(&r).contains("abc"));
        // the base entries stay
        assert!(format_acl(&r).starts_with("user::rwx,group::r-x"));
    }

    #[test]
    fn set_bits_goes_to_the_mask() {
        let mut v = normalize(parse_acl("user::rwx,group::r-x,other::---,user:abc:rwx").unwrap(), &[]);
        set_permission_bits(&mut v, 0o640);
        let s = format_acl(&v);
        assert!(s.contains("group::r-x") && s.contains("mask::r--"), "{s}");
        assert_eq!(permission_bits(&v), 0o640);
    }
}
