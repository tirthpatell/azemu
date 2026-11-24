//! Names: what a share, a directory and a file may be called, and how a request path becomes a list of names.

use crate::errors;
use azemu_core::{Request, RequestCtx, Response};

/// Characters a directory or file name may not hold (Windows rules, which Azure Files follows).
const FORBIDDEN: &[char] = &['"', '/', ':', '|', '<', '>', '*', '?'];
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
    "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
/// The longest name of one directory or file.
pub const MAX_NAME: usize = 255;
/// The longest path (all the names, with the separators).
pub const MAX_PATH: usize = 2048;
/// The most directories deep.
pub const MAX_DEPTH: usize = 250;

#[derive(Debug, PartialEq, Eq)]
pub enum NameError {
    /// 400 `OutOfRangeInput`
    Length,
    /// 400 `InvalidResourceName`
    Invalid,
    /// 400 `InvalidFileOrDirectoryPathName`: more than 250 names deep.
    TooDeep,
    /// 400 `InvalidFileOrDirectoryPathName`: more than 2,048 characters.
    TooLong,
}

/// A share name: 3 to 63 characters, lower-case letters, digits and single hyphens, no hyphen at either end.
pub fn validate_share(name: &str) -> Result<(), NameError> {
    if !(3..=63).contains(&name.len()) {
        return Err(NameError::Length);
    }
    let ok = name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(NameError::Invalid)
    }
}

/// The name of a directory or file (after trailing dots are dealt with).
pub fn validate_entry(name: &str) -> Result<(), NameError> {
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(NameError::Invalid);
    }
    if name.chars().any(|c| c < ' ' || FORBIDDEN.contains(&c)) {
        return Err(NameError::Invalid);
    }
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name)) {
        return Err(NameError::Invalid);
    }
    Ok(())
}

pub fn share_error(ctx: &RequestCtx, e: NameError) -> Response {
    match e {
        NameError::Length => errors::name_out_of_range(ctx),
        NameError::Invalid => errors::invalid_resource_name(ctx),
        NameError::TooDeep => {
            errors::err(ctx, 400, "InvalidFileOrDirectoryPathName", "File or directory path has too many subdirectories.")
        }
        NameError::TooLong => errors::err(ctx, 400, "InvalidFileOrDirectoryPathName", "File or directory path is too long."),
    }
}

/// Does the request keep trailing dots (`x-ms-allow-trailing-dot: true`)? Without it Azure drops them from every name.
pub fn allow_trailing_dot(req: &Request) -> bool {
    req.header("x-ms-allow-trailing-dot").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Split a path inside a share (`a/b\c`) into names. A backslash separates like a slash. Trailing dots are cut off unless asked
/// for. The names are not checked.
pub fn split_path(path: &str, keep_trailing_dots: bool) -> Vec<String> {
    path.split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .map(|s| if keep_trailing_dots { s.to_string() } else { s.trim_end_matches('.').to_string() })
        .collect()
}

/// Check every name of a path and the limits on its size.
pub fn validate_path(names: &[String]) -> Result<(), NameError> {
    names.iter().try_for_each(|n| validate_entry(n))?;
    if names.len() > MAX_DEPTH {
        return Err(NameError::TooDeep);
    }
    if names.iter().map(|n| n.chars().count() + 1).sum::<usize>() > MAX_PATH {
        return Err(NameError::TooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_names() {
        for ok in ["abc", "a-b", "1ab", "con", &"a".repeat(63)] {
            assert_eq!(validate_share(ok), Ok(()), "{ok}");
        }
        for bad in ["Abc", "a--b", "-ab", "ab-", "a_b", "a.b", "a b"] {
            assert_eq!(validate_share(bad), Err(NameError::Invalid), "{bad}");
        }
        for bad in ["ab", "", &"a".repeat(64)] {
            assert_eq!(validate_share(bad), Err(NameError::Length), "{bad}");
        }
    }

    #[test]
    fn entry_names() {
        for ok in ["a", "a b", "con.txt", "$a", "ünï", "a#b", "a%b", &"x".repeat(255)] {
            assert_eq!(validate_entry(ok), Ok(()), "{ok}");
        }
        for bad in ["CON", "aux", "Nul", "com1", "a:b", "a*b", "a?b", "a<b", "a|b", "a\u{1}b", &"x".repeat(256), ""] {
            assert!(validate_entry(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn path_limits() {
        let deep: Vec<String> = vec!["d".into(); 251];
        assert_eq!(validate_path(&deep), Err(NameError::TooDeep));
        assert_eq!(validate_path(&deep[..250]), Ok(()));
        let long: Vec<String> = vec!["x".repeat(200); 11];
        assert_eq!(validate_path(&long), Err(NameError::TooLong));
    }

    #[test]
    fn paths() {
        assert_eq!(split_path("a/b\\c//d", false), ["a", "b", "c", "d"]);
        assert_eq!(split_path("a./b..", false), ["a", "b"]);
        assert_eq!(split_path("a./b..", true), ["a.", "b.."]);
    }
}
