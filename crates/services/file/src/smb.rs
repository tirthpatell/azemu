//! The SMB properties of files and directories: attributes, the three times and the permission, read from the `x-ms-file-*` headers
//! with their key words (`preserve`, `now`, `inherit`), and written back as response headers.

use azemu_core::resp::Resp;
use azemu_core::time::{iso8601_7, parse_iso8601, OffsetDateTime};
use azemu_core::{Request, Response};
use sha2::{Digest, Sha256};

use crate::errors;
use std::collections::BTreeMap;

use crate::state::{attr, Smb, DEFAULT_DIR_KEY, DEFAULT_FILE_KEY};

/// The security descriptors an account holds, by key.
pub type Perms = BTreeMap<String, String>;

/// A value a client may leave out, keep (`preserve`), set to now (`now`) or give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick<T> {
    Absent,
    Preserve,
    Now,
    Value(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Perm {
    Absent,
    Preserve,
    Inherit,
    Key(String),
    Sddl(String),
}

#[derive(Debug, Clone)]
pub struct SmbIn {
    pub attributes: Pick<u32>,
    pub creation: Pick<OffsetDateTime>,
    pub last_write: Pick<OffsetDateTime>,
    pub change: Pick<OffsetDateTime>,
    pub permission: Perm,
}

/// What the headers ask for. `directory` decides what attributes are valid.
/// `allow_now`: Create, Copy and Rename take `now` for a time; Set Properties does not (Azure answers `InvalidHeaderValue`).
pub fn read(req: &Request, directory: bool, allow_now: bool) -> Result<SmbIn, Response> {
    let ctx = &req.ctx;
    let bad = |name: &str, v: &str| errors::invalid_header(ctx, name, v);

    let attributes = match req.header("x-ms-file-attributes") {
        None => Pick::Absent,
        Some(v) => {
            let t = v.trim();
            if t.eq_ignore_ascii_case("preserve") {
                Pick::Preserve
            } else {
                Pick::Value(parse_attributes(t, directory).ok_or_else(|| bad("x-ms-file-attributes", &v.to_ascii_lowercase()))?)
            }
        }
    };
    let time = |name: &str| -> Result<Pick<OffsetDateTime>, Response> {
        match req.header(name) {
            None => Ok(Pick::Absent),
            Some(v) => {
                let t = v.trim();
                if t.eq_ignore_ascii_case("preserve") {
                    Ok(Pick::Preserve)
                } else if t.eq_ignore_ascii_case("now") && allow_now {
                    Ok(Pick::Now)
                } else {
                    parse_iso8601(t).map(Pick::Value).ok_or_else(|| bad(name, v))
                }
            }
        }
    };
    let creation = time("x-ms-file-creation-time")?;
    let last_write = time("x-ms-file-last-write-time")?;
    let change = time("x-ms-file-change-time")?;

    let permission = match (req.header("x-ms-file-permission"), req.header("x-ms-file-permission-key")) {
        (Some(_), Some(_)) => {
            return Err(errors::err(
                ctx,
                400,
                "BothPermissionAndPermissionKeyHeaderPresent",
                "Both x-ms-file-permission header and x-ms-file-permission-key header are present.",
            ))
        }
        (Some(p), None) => {
            let p = p.trim();
            if p.eq_ignore_ascii_case("inherit") {
                Perm::Inherit
            } else if p.eq_ignore_ascii_case("preserve") {
                Perm::Preserve
            } else if valid_sddl(p) || (is_binary(req) && valid_binary(p)) {
                Perm::Sddl(p.to_string())
            } else {
                return Err(errors::err(ctx, 400, "FileInvalidPermission", "The specified file permission is not valid."));
            }
        }
        (None, Some(k)) => Perm::Key(k.trim().to_string()),
        (None, None) => Perm::Absent,
    };
    Ok(SmbIn { attributes, creation, last_write, change, permission })
}

/// `x-ms-file-permission-format: Binary`: the permission is a security descriptor in its binary form, in base64.
pub fn is_binary(req: &Request) -> bool {
    req.header("x-ms-file-permission-format").is_some_and(|f| f.trim().eq_ignore_ascii_case("binary"))
}

pub fn valid_binary(s: &str) -> bool {
    use base64::Engine;
    s.len() <= 16 * 1024 && base64::engine::general_purpose::STANDARD.decode(s).is_ok_and(|b| b.len() > 20)
}

/// Is this a security descriptor in SDDL form? (It starts with one of the four sections; the text is not parsed further.)
pub fn valid_sddl(s: &str) -> bool {
    s.len() <= 8 * 1024 && matches!(s.get(..2), Some("O:" | "G:" | "D:" | "S:"))
}

/// `Hidden|Archive`: names joined by `|`, in any case. A directory's attributes must hold `Directory` or be `None`; a file's may not.
pub fn parse_attributes(s: &str, directory: bool) -> Option<u32> {
    let mut bits = 0u32;
    for part in s.split('|') {
        let p = part.trim();
        let (_, b) = attr::NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(p))?;
        bits |= b;
    }
    if bits & attr::NORMAL != 0 && bits != attr::NORMAL {
        // `None` means no other attribute (Azure refuses `Hidden|None`)
        return None;
    }
    if directory {
        if bits == attr::NORMAL {
            return Some(attr::DIRECTORY);
        }
        if bits & attr::DIRECTORY == 0 {
            return None;
        }
    } else if bits & attr::DIRECTORY != 0 {
        return None;
    }
    Some(bits)
}

/// The key a permission is stored under: two numbers from its hash, joined by `*` like Azure's.
pub fn permission_key(sddl: &str) -> String {
    let h = Sha256::digest(sddl.as_bytes());
    let a = u64::from_be_bytes(h[0..8].try_into().unwrap_or([0; 8]));
    format!("{a}*3227211760911779636")
}

fn key_not_found(req: &Request) -> Response {
    errors::err(&req.ctx, 400, "FileInvalidPermissionKey", "The specified file permission key does not exist in the share.")
}

/// Resolve the permission a request names to a key (storing a new descriptor), `None` for "keep what is there".
fn resolve_permission(perms: &mut Perms, req: &Request, perm: &Perm, parent_key: &str) -> Result<Option<String>, Response> {
    Ok(match perm {
        Perm::Absent | Perm::Preserve => None,
        Perm::Inherit => Some(parent_key.to_string()),
        Perm::Key(k) => {
            let known = k == DEFAULT_DIR_KEY || k == DEFAULT_FILE_KEY || perms.contains_key(k);
            if !known {
                return Err(key_not_found(req));
            }
            Some(k.clone())
        }
        Perm::Sddl(s) => {
            let k = permission_key(s);
            perms.insert(k.clone(), s.clone());
            Some(k)
        }
    })
}

fn pick_time(p: &Pick<OffsetDateTime>, now: OffsetDateTime, keep: OffsetDateTime) -> OffsetDateTime {
    match p {
        Pick::Value(t) => *t,
        Pick::Now => now,
        Pick::Preserve | Pick::Absent => keep,
    }
}

/// The properties of a new file or directory. What is left out is now, `Archive` for a file and `Directory` for a directory, and
/// the permission the parent has.
pub fn for_new(
    perms: &mut Perms,
    req: &Request,
    inp: &SmbIn,
    directory: bool,
    parent_key: &str,
    now: OffsetDateTime,
) -> Result<Smb, Response> {
    let default_key = if directory { DEFAULT_DIR_KEY } else { DEFAULT_FILE_KEY };
    let key = resolve_permission(perms, req, &inp.permission, parent_key)?.unwrap_or_else(|| default_key.to_string());
    let attributes = match inp.attributes {
        Pick::Value(v) if v == attr::NORMAL => attr::ARCHIVE,
        Pick::Value(v) => v,
        _ if directory => attr::DIRECTORY,
        _ => attr::ARCHIVE,
    };
    let creation = pick_time(&inp.creation, now, now);
    Ok(Smb {
        attributes,
        creation,
        last_write: pick_time(&inp.last_write, now, now),
        change: pick_time(&inp.change, now, now),
        permission_key: key,
    })
}

/// Apply Set Properties: what is left out or `preserve` stays.
pub fn set(perms: &mut Perms, req: &Request, smb: &mut Smb, inp: &SmbIn, now: OffsetDateTime) -> Result<(), Response> {
    if let Some(k) = resolve_permission(perms, req, &inp.permission, &smb.permission_key.clone())? {
        smb.permission_key = k;
    }
    if let Pick::Value(v) = inp.attributes {
        smb.attributes = v;
    }
    if inp.creation != Pick::Absent {
        smb.creation = pick_time(&inp.creation, now, smb.creation);
    }
    if inp.last_write != Pick::Absent {
        smb.last_write = pick_time(&inp.last_write, now, smb.last_write);
    }
    smb.change = match inp.change {
        Pick::Value(t) => t,
        _ => now,
    };
    Ok(())
}

/// The headers that carry the SMB properties, as Create, Set Properties and the reads send them.
pub fn headers(mut r: Resp, smb: &Smb, id: u64, parent_id: u64) -> Resp {
    r = r
        .header("x-ms-file-change-time", iso8601_7(smb.change))
        .header("x-ms-file-last-write-time", iso8601_7(smb.last_write))
        .header("x-ms-file-creation-time", iso8601_7(smb.creation))
        .header("x-ms-file-permission-key", &smb.permission_key)
        .header("x-ms-file-attributes", attr::render(smb.attributes))
        .header("x-ms-file-id", id)
        .header("x-ms-file-parent-id", parent_id);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes() {
        assert_eq!(parse_attributes("Hidden|Directory", true), Some(attr::HIDDEN | attr::DIRECTORY));
        assert_eq!(parse_attributes("hidden | directory", true), Some(attr::HIDDEN | attr::DIRECTORY));
        assert_eq!(parse_attributes("None", true), Some(attr::DIRECTORY));
        assert_eq!(parse_attributes("Hidden", true), None);
        assert_eq!(parse_attributes("Normal", false), None);
        assert_eq!(parse_attributes("ReadOnly|Archive", false), Some(attr::READ_ONLY | attr::ARCHIVE));
        assert_eq!(parse_attributes("Directory", false), None);
        assert_eq!(parse_attributes("None", false), Some(attr::NORMAL));
        assert_eq!(attr::render(attr::HIDDEN | attr::DIRECTORY), "Hidden | Directory");
        assert_eq!(attr::render(attr::NORMAL), "None");
    }

    #[test]
    fn sddl_and_keys() {
        assert!(valid_sddl("O:S-1-5-21G:S-1-5-21D:(A;;FA;;;SY)"));
        assert!(!valid_sddl("garbage") && !valid_sddl("preserve") && !valid_sddl(""));
        assert_eq!(permission_key("O:AB"), permission_key("O:AB"));
        assert_ne!(permission_key("O:AB"), permission_key("O:AC"));
        assert!(permission_key("O:AB").contains('*'));
    }
}
