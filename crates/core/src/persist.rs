//! Saving state to disk and reading it back (`--data-dir`).
//!
//! The contract is three methods on [`Service`](crate::Service): `persist_generation`, `save` and
//! `load`. Core decides when they run; a service decides what its files hold. This module has the
//! helpers every service uses, so that all state files share one format and one way of being
//! replaced:
//!
//! * a state file starts with a one-line header `AZEMU-STATE <kind> <version>\n`, followed by the
//!   value in MessagePack (structs as maps with field names, so a field can be added later with
//!   `#[serde(default)]` without breaking old files);
//! * a file is always replaced as a whole: written to a temporary file next to it, flushed to disk,
//!   then renamed over the old one. A crash during a save leaves the previous save readable.
//!
//! `kind` names what the file holds (`"blob"`, `"queue"`); `version` is the service's own number,
//! raised when an old file can no longer be read. A file of another kind or a newer version is an
//! error that names the file, never silently ignored.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;

const MAGIC: &str = "AZEMU-STATE";
/// The deepest nesting of arrays and maps `decode` accepts. A JSON request is limited to 128 levels, so a stored
/// document never needs more than that plus the levels of the state around it.
pub const MAX_DEPTH: usize = 200;

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The bytes of a state file for `value`.
pub fn encode<T: Serialize>(kind: &str, version: u32, value: &T) -> io::Result<Vec<u8>> {
    let mut out = format!("{MAGIC} {kind} {version}\n").into_bytes();
    rmp_serde::encode::write_named(&mut out, value).map_err(|e| invalid(format!("cannot encode {kind} state: {e}")))?;
    Ok(out)
}

/// Read a state file made by [`encode`]. `max_version` is the newest version this build reads; the
/// version found in the file is returned with the value so a service can read its older layouts.
pub fn decode<T: DeserializeOwned>(kind: &str, max_version: u32, bytes: &[u8]) -> io::Result<(u32, T)> {
    let end = bytes
        .iter()
        .take(200)
        .position(|b| *b == b'\n')
        .ok_or_else(|| invalid(format!("not an azemu state file (no header line); expected {kind} state")))?;
    let header = std::str::from_utf8(&bytes[..end]).map_err(|_| invalid("state file header is not text".into()))?;
    let mut parts = header.split(' ');
    let (magic, found_kind, version) = (parts.next(), parts.next(), parts.next());
    if magic != Some(MAGIC) {
        return Err(invalid(format!("not an azemu state file; expected {kind} state")));
    }
    if found_kind != Some(kind) {
        return Err(invalid(format!("state file holds {} state, expected {kind}", found_kind.unwrap_or("no"))));
    }
    let version: u32 = version.and_then(|v| v.parse().ok()).ok_or_else(|| invalid(format!("{kind} state file has no version number")))?;
    if version > max_version {
        return Err(invalid(format!(
            "{kind} state file has version {version}; this azemu reads up to version {max_version} (it was written by a newer azemu)"
        )));
    }
    let mut de = rmp_serde::Deserializer::new(&bytes[end + 1..]);
    // nesting deeper than any request can make is not state: it would only be a way to overflow the stack
    // (the reader's own limit is 1024 levels, which is more than a 2 MiB thread stack holds in a debug build)
    de.set_max_depth(MAX_DEPTH);
    let value = T::deserialize(&mut de).map_err(|e| invalid(format!("cannot read {kind} state (version {version}): {e}")))?;
    Ok((version, value))
}

/// Replace `path` with `bytes` as a whole: a reader sees the old file or the new one, never a part.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_inner(path, bytes, false)
}

/// Like [`write_atomic`], but the file is created readable and writable by the owner only (mode 0600 on Unix).
/// For state that holds secrets, such as private keys.
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_inner(path, bytes, true)
}

fn write_atomic_inner(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| invalid(format!("{} has no parent directory", path.display())))?;
    let name = path.file_name().ok_or_else(|| invalid(format!("{} has no file name", path.display())))?;
    let tmp = dir.join(format!(".{}.tmp-{}", name.to_string_lossy(), std::process::id()));
    let result = (|| {
        let mut f = create_new_file(&tmp, private)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        // make the rename itself durable; not every platform lets a directory be opened for this
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

fn create_new_file(path: &Path, private: bool) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    o.open(path)
}

/// The bytes of `path`, or `None` when there is no such file.
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
}

/// [`encode`] then [`write_atomic`] into `dir/file`.
pub fn save_file<T: Serialize>(dir: &Path, file: &str, kind: &str, version: u32, value: &T) -> io::Result<()> {
    write_atomic(&dir.join(file), &encode(kind, version, value)?)
}

/// Like [`save_file`], but nothing is written when the file already holds exactly these bytes. Returns whether it
/// wrote. Only useful when `value` encodes the same way every time (use `BTreeMap`, not `HashMap`). `private`:
/// create the file with mode 0600.
pub fn save_file_if_changed<T: Serialize>(dir: &Path, file: &str, kind: &str, version: u32, value: &T, private: bool) -> io::Result<bool> {
    let bytes = encode(kind, version, value)?;
    let path = dir.join(file);
    if let Ok(old) = fs::read(&path) {
        if old == bytes {
            return Ok(false);
        }
    }
    write_atomic_inner(&path, &bytes, private)?;
    Ok(true)
}

/// Like [`save_file`], with [`write_atomic_private`].
pub fn save_file_private<T: Serialize>(dir: &Path, file: &str, kind: &str, version: u32, value: &T) -> io::Result<()> {
    write_atomic_private(&dir.join(file), &encode(kind, version, value)?)
}

/// [`read_optional`] then [`decode`] from `dir/file`. `None` when the file does not exist. An error
/// names the file.
pub fn load_file<T: DeserializeOwned>(dir: &Path, file: &str, kind: &str, max_version: u32) -> io::Result<Option<(u32, T)>> {
    let path = dir.join(file);
    match read_optional(&path)? {
        None => Ok(None),
        Some(bytes) => {
            decode(kind, max_version, &bytes).map(Some).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        name: String,
        data: bytes::Bytes,
        doc: serde_json::Value,
        #[serde(default)]
        added_later: Option<u32>,
    }

    fn sample() -> Sample {
        Sample {
            name: "a".into(),
            data: bytes::Bytes::from_static(b"\x00\x01binary\xff"),
            doc: serde_json::json!({"id": "1", "n": 1.5, "list": [1, null, {"x": true}]}),
            added_later: Some(7),
        }
    }

    #[test]
    fn round_trip_keeps_bytes_and_json() {
        let bytes = encode("sample", 3, &sample()).unwrap();
        assert!(bytes.starts_with(b"AZEMU-STATE sample 3\n"));
        let (version, back): (u32, Sample) = decode("sample", 3, &bytes).unwrap();
        assert_eq!(version, 3);
        assert_eq!(back, sample());
    }

    #[test]
    fn a_field_added_later_reads_old_files() {
        #[derive(Serialize)]
        struct Old {
            name: String,
            data: bytes::Bytes,
            doc: serde_json::Value,
        }
        let old = Old { name: "a".into(), data: bytes::Bytes::new(), doc: serde_json::Value::Null };
        let bytes = encode("sample", 1, &old).unwrap();
        let (version, back): (u32, Sample) = decode("sample", 2, &bytes).unwrap();
        assert_eq!((version, back.added_later), (1, None));
    }

    #[test]
    fn wrong_kind_newer_version_and_garbage_are_errors_that_say_why() {
        let bytes = encode("sample", 5, &sample()).unwrap();
        let e = decode::<Sample>("other", 5, &bytes).unwrap_err().to_string();
        assert!(e.contains("holds sample state, expected other"), "{e}");
        let e = decode::<Sample>("sample", 4, &bytes).unwrap_err().to_string();
        assert!(e.contains("version 5") && e.contains("newer azemu"), "{e}");
        assert!(decode::<Sample>("sample", 5, b"junk without a header").is_err());
        assert!(decode::<Sample>("sample", 5, &bytes[..bytes.len() - 3]).is_err());
    }

    #[test]
    fn deeply_nested_input_is_an_error_not_a_stack_overflow() {
        let nested = |depth: usize| {
            let mut bytes = b"AZEMU-STATE deep 1\n".to_vec();
            bytes.extend(std::iter::repeat_n(0x91u8, depth));
            bytes.push(0xc0);
            bytes
        };
        assert!(decode::<serde_json::Value>("deep", 1, &nested(50)).is_ok());
        let e = decode::<serde_json::Value>("deep", 1, &nested(100_000)).unwrap_err().to_string();
        assert!(e.contains("depth limit"), "{e}");
        assert!(decode::<serde_json::Value>("deep", 1, &nested(MAX_DEPTH + 10)).is_err());
    }

    #[test]
    fn a_private_file_is_created_with_mode_0600() {
        let dir = std::env::temp_dir().join(format!("azemu-persist-private-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save_file_private(&dir, "p.state", "sample", 1, &sample()).unwrap();
        save_file(&dir, "q.state", "sample", 1, &sample()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(dir.join("p.state")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // nothing is written when the bytes are the same
        assert!(!save_file_if_changed(&dir, "q.state", "sample", 1, &sample(), false).unwrap());
        assert!(save_file_if_changed(&dir, "q.state", "sample", 2, &sample(), false).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temporary_file() {
        let dir = std::env::temp_dir().join(format!("azemu-persist-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        save_file(&dir, "s.state", "sample", 1, &sample()).unwrap();
        save_file(&dir, "s.state", "sample", 1, &sample()).unwrap();
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let (_, back): (u32, Sample) = load_file(&dir, "s.state", "sample", 1).unwrap().unwrap();
        assert_eq!(back, sample());
        assert!(load_file::<Sample>(&dir, "missing.state", "sample", 1).unwrap().is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
