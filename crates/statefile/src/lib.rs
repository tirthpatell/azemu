//! Helpers for the state files of Service Bus and Event Hubs (`--data-dir`, see `docs/services/persistence.md`):
//! the common file format of `azemu_core::persist`, written straight to the file so that a queue of hundreds of
//! megabytes is not also held as one buffer, with owner-only permissions.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A state file larger than this is refused before it is read (a damaged or foreign file must not fill the memory).
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn invalid(path: &Path, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {msg}", path.display()))
}

/// Write `value` to `path` as a state file of `kind` and `version`: a header line, then MessagePack with field names.
/// Written to a temporary file next to it (mode 0600), flushed to disk, then renamed over the old file, so a reader
/// (or a crash) sees the old file or the new one, never a part.
pub fn write_stream<T: Serialize>(path: &Path, kind: &str, version: u32, value: &T) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| invalid(path, "no directory"))?;
    let name = path.file_name().ok_or_else(|| invalid(path, "no file name"))?;
    let tmp = dir.join(format!(".{}.tmp-{}", name.to_string_lossy(), std::process::id()));
    let result = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let f = opts.open(&tmp)?;
        let mut w = io::BufWriter::with_capacity(1 << 20, f);
        w.write_all(format!("AZEMU-STATE {kind} {version}\n").as_bytes())?;
        rmp_serde::encode::write_named(&mut w, value)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("cannot encode {kind} state: {e}")))?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Read a state file written by [`write_stream`] (or `azemu_core::persist`). `None` when there is no such file. An
/// error names the file. The version found is returned with the value.
pub fn read_state<T: DeserializeOwned>(path: &Path, kind: &str, max_version: u32) -> io::Result<Option<(u32, T)>> {
    match fs::metadata(path) {
        Ok(m) if m.len() > MAX_FILE_BYTES => {
            return Err(invalid(path, format!("the file is {} bytes, more than the {MAX_FILE_BYTES} a state file may have", m.len())))
        }
        Ok(m) if !m.is_file() => return Err(invalid(path, "not a regular file")),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
    let Some(bytes) = azemu_core::persist::read_optional(path)? else { return Ok(None) };
    azemu_core::persist::decode(kind, max_version, &bytes)
        .map(Some)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// `prefix-<16 hex digits>.state`, the digits a hash of `parts`. What a file holds says which entity it is, so two
/// names with the same hash cannot be mixed up (the file would be seen as belonging to another entity).
pub fn hashed_name(prefix: &str, parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for part in parts {
        for b in part.bytes().chain(std::iter::once(0xff)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{prefix}-{h:016x}.state")
}

/// Remove the files `prefix-*.state` in `dir` that are not in `keep`.
pub fn remove_stale(dir: &Path, prefix: &str, keep: &HashSet<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{prefix}-")) && name.ends_with(".state") && !keep.contains(&name) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        name: String,
        data: Vec<u8>,
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("azemu-statefile-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_written_file_reads_back_is_private_and_leaves_no_temporary_file() {
        let dir = scratch("roundtrip");
        let v = Sample { name: "a".into(), data: vec![0, 255, 7] };
        write_stream(&dir.join("s.state"), "sample", 2, &v).unwrap();
        write_stream(&dir.join("s.state"), "sample", 2, &v).unwrap();
        let (version, back): (u32, Sample) = read_state(&dir.join("s.state"), "sample", 2).unwrap().unwrap();
        assert_eq!((version, back), (2, v));
        assert!(read_state::<Sample>(&dir.join("none.state"), "sample", 2).unwrap().is_none());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(dir.join("s.state")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // another kind and a newer version are errors that name the file
        let e = read_state::<Sample>(&dir.join("s.state"), "other", 2).unwrap_err().to_string();
        assert!(e.contains("s.state") && e.contains("holds sample state"), "{e}");
        let e = read_state::<Sample>(&dir.join("s.state"), "sample", 1).unwrap_err().to_string();
        assert!(e.contains("s.state") && e.contains("newer azemu"), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_are_stable_and_stale_files_go() {
        assert_eq!(hashed_name("q", &["a", "b"]), hashed_name("q", &["a", "b"]));
        assert_ne!(hashed_name("q", &["a", "b"]), hashed_name("q", &["ab", ""]), "parts are kept apart");
        let dir = scratch("stale");
        let keep_name = hashed_name("q", &["keep"]);
        for n in [keep_name.clone(), hashed_name("q", &["old"]), "other.state".to_string(), "q-notes.txt".to_string()] {
            fs::write(dir.join(n), b"x").unwrap();
        }
        remove_stale(&dir, "q", &HashSet::from([keep_name.clone()])).unwrap();
        let mut left: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let mut want = vec![keep_name, "other.state".to_string(), "q-notes.txt".to_string()];
        want.sort();
        assert_eq!(left, want);
        fs::remove_dir_all(&dir).unwrap();
    }
}
