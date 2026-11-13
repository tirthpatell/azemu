//! Directories that hold secrets (the certificate authority's key, saved Key Vault keys): created for their owner
//! alone, whatever the umask of the process says.

use std::io;
use std::path::Path;

/// Make `dir` when it is missing, with mode 0700, and say whether it was made here. A directory that exists is left
/// as it is: the user may have chosen its mode. Missing parent directories get the usual modes; only `dir` itself holds
/// the secrets. On systems without Unix modes this is `create_dir_all`.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<bool> {
    if dir.is_dir() {
        return Ok(false);
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {
                // the mode given to `create` is cut down by the umask (it can only remove rights), so set it once more
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(false),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map(|_| true)
    }
}

/// A one-line warning when `dir` exists and group or other users can use it (a directory the user chose is never
/// changed, so all we can do is say so). `what` names the directory's role, for example "the certificate directory".
pub(crate) fn open_directory_warning(dir: &Path, what: &str, holds: &str) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir).ok()?.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            return None;
        }
        let writable = mode & 0o022 != 0;
        let risk = if writable { "other users can replace what it holds" } else { "other users can look into it" };
        Some(format!(
            "{what} {} has mode {mode:03o}, so {risk}; it holds {holds}. azemu does not change a directory that already exists: run chmod 700 on it, unless you share it on purpose",
            dir.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, what, holds);
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("azemu-private-dir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_new_directory_is_private_and_so_is_its_own_creation_below_missing_parents() {
        let base = scratch("new");
        let dir = base.join("missing").join("ca");
        assert!(create_private_dir(&dir).unwrap());
        assert_eq!(mode(&dir), 0o700);
        assert!(!create_private_dir(&dir).unwrap(), "a second call finds it");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_directory_that_exists_is_left_alone_and_a_wide_one_is_reported() {
        let base = scratch("old");
        let dir = base.join("shared");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(!create_private_dir(&dir).unwrap());
        assert_eq!(mode(&dir), 0o775);
        let w = open_directory_warning(&dir, "the certificate directory", "the certificate authority's key").unwrap();
        assert!(w.contains("775") && w.contains("replace") && !w.contains('\n'), "{w}");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(open_directory_warning(&dir, "the data directory", "keys").unwrap().contains("look into"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(open_directory_warning(&dir, "the data directory", "keys"), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
