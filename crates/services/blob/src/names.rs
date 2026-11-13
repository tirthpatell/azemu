//! The rules for the name of a blob or a Data Lake path, in one place for both endpoints.
//!
//! Source: "Naming and Referencing Containers, Blobs, and Metadata" (Microsoft Learn):
//! * a name is 1 to 1,024 characters, and "control characters (0x00 to 0x1F, \u0081, etc.) are also not allowed" (a name that
//!   is not valid UTF-8 reaches us as U+FFFD, which the page lists among the characters that "fail UTF-8 or MBCS decoding"; two
//!   different invalid names must not become one, so U+FFFD is refused too: `400 InvalidResourceName`). U+FFFF, which the same list
//!   names, is **accepted**: the recording `test_list_encoded_blobs` (Python blob package) shows Azure answering `201` for a blob
//!   named `file\u{FFFF}.blob`, so a list of "not recommended" characters is not a list of refused ones;
//! * "if the storage account does not have hierarchical namespace enabled, the number of path segments comprising the blob name
//!   cannot exceed 254; if it has, it cannot exceed 63 (including path segments for account name and container name)", which
//!   leaves 61 for the name inside a container of a hierarchical account;
//! * a path segment must not be `.` or `..` (the page says to avoid segments that end in a dot; clients and proxies resolve those
//!   two before they send the request, so no client could address such a name again). This is checked in hierarchical containers
//!   only, where the segments are directories.
//!
//! In a hierarchical container (Data Lake) a blob also needs its parents to be directories: writing `a.txt/child` when `a.txt` is a
//! file is `409 PathConflict`, the code that the Data Lake error table describes with "the specified path, or an element of the
//! path, exists and its resource type is invalid for this operation" (the Blob endpoint's own answer for this case is not in
//! any recording; the Data Lake code is a guess for it).

use crate::dfs::model::{dir_attrs_of, is_dir, live, path_conflict};
use crate::errors::{err, ApiError};
use crate::state::Container;

/// Most characters in a name.
pub const MAX_CHARS: usize = 1024;
/// Most path segments in a name in a container of an account without a hierarchical namespace.
pub const MAX_SEGMENTS_FLAT: usize = 254;
/// Most path segments in a name inside a container of a hierarchical account (63 with the account and the container).
pub const MAX_SEGMENTS_HIERARCHICAL: usize = 61;

fn bad(why: &str) -> ApiError {
    err("InvalidResourceName").msg(format!("The specifed resource name contains invalid characters: {why}"))
}

/// The rules for the text of a name. `hierarchical`: the container belongs to a hierarchical account (a Data Lake filesystem).
pub fn check(name: &str, hierarchical: bool) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(bad("the name is empty."));
    }
    // Counting stops at the limit, so a long name is not walked to its end.
    if name.chars().take(MAX_CHARS + 1).count() > MAX_CHARS {
        return Err(bad("the name is longer than 1,024 characters."));
    }
    if name.chars().any(|c| c < '\u{20}' || c == '\u{FFFD}') {
        return Err(bad("control characters and bytes that are not valid UTF-8 are not allowed."));
    }
    let segments = name.split('/').count();
    if hierarchical {
        if segments > MAX_SEGMENTS_HIERARCHICAL {
            return Err(bad("a path in a hierarchical namespace has at most 61 segments below the container."));
        }
        if name.split('/').any(|s| s == "." || s == "..") {
            return Err(bad("a path segment cannot be '.' or '..'."));
        }
    } else if segments > MAX_SEGMENTS_FLAT {
        return Err(bad("the name has more than 254 path segments."));
    }
    Ok(())
}

/// A write through the Blob endpoint into a hierarchical container: every parent has to be a directory (or absent: the Data Lake
/// side makes it), and a directory cannot be replaced by a blob. `creates` is false for staging a block, which does not replace
/// anything yet.
pub fn check_parents(c: &Container, name: &str, creates: bool) -> Result<(), ApiError> {
    let mut end = 0;
    for part in name.split('/') {
        end += part.len();
        if end < name.len() {
            if let Some(b) = live(c, &name[..end]) {
                if !is_dir(b) {
                    return Err(path_conflict());
                }
            }
            end += 1;
        }
    }
    if creates && dir_attrs_of(c, name).is_some() {
        return Err(path_conflict());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_rules() {
        assert!(check("a/b/c.txt", false).is_ok() && check("a/b/c.txt", true).is_ok());
        assert!(check("", false).is_err());
        assert!(check(&"x".repeat(1024), false).is_ok());
        assert!(check(&"x".repeat(1025), false).is_err());
        assert!(check(&"é".repeat(1024), true).is_ok(), "characters, not bytes");
        for bad in ["a\0b", "a\nb", "a\u{1f}", "a\u{fffd}b"] {
            assert_eq!(check(bad, false).unwrap_err().code, "InvalidResourceName", "{bad:?}");
            assert!(check(bad, true).is_err());
        }
        assert!(check("tab\there", false).is_err());
        // recorded: Azure accepts U+FFFF in a blob name
        assert!(check("dir1/dir2/file\u{ffff}.blob", false).is_ok() && check("dir1/dir2/file\u{ffff}.blob", true).is_ok());
        assert!(check("日本語/ファイル", true).is_ok());
        // segments
        assert!(check(&vec!["s"; 254].join("/"), false).is_ok());
        assert!(check(&vec!["s"; 255].join("/"), false).is_err());
        assert!(check(&vec!["s"; 61].join("/"), true).is_ok());
        assert!(check(&vec!["s"; 62].join("/"), true).is_err());
        assert!(check(&vec!["s"; 62].join("/"), false).is_ok());
        // dots: only in a hierarchical container
        assert!(check("a/../b", true).is_err() && check("./a", true).is_err() && check("a/.", true).is_err());
        assert!(check("a/../b", false).is_ok());
        assert!(check("a/.b/..c/d.", true).is_ok());
    }
}
