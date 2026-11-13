//! The language between the emulator and a script worker process.
//!
//! Messages are JSON, each preceded by its length as four bytes (big endian), over the worker's standard input (emulator to
//! worker) and standard output (worker to emulator). Standard error is for diagnostics only (a panic or an out-of-memory
//! message of the Rust runtime); the emulator reads a bounded part of it after the worker has ended, to say why.
//!
//! The worker may ask the emulator for exactly one kind of thing, a collection call ([`FromWorker::Host`]: create, read,
//! query, replace, upsert, delete, the feeds, and log lines), and the emulator answers each with one [`ToWorker::Reply`]. The
//! worker never gets a file, a socket or the state of the service. Everything the worker sends is untrusted: the emulator
//! bounds the size of every message before it reads it, and the number and cost of the collection calls are bounded by the
//! host that answers them.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Failure, Outcome};

/// Largest message the emulator accepts from a worker. The response body of a script is at most 2 MB, which JSON-escaped is
/// less than twice that; the rest of the room is for collection calls (a document written by a script is at most 2 MB too).
pub const MAX_FROM_WORKER: usize = 16 * 1024 * 1024;
/// Largest message a worker accepts from the emulator. An answer bigger than [`MAX_REPLY`] is not sent as it is.
pub const MAX_TO_WORKER: usize = 80 * 1024 * 1024;
/// Largest answer to a collection call the emulator sends (a feed or a query result as JSON text).
pub const MAX_REPLY: usize = 48 * 1024 * 1024;

/// What a worker is started with (the first argument after the hidden word, as JSON): the limits it applies to itself and
/// the engine settings. It is a command line argument, not a message, so a warm worker has the engine ready before its first job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// What the worker may add to its address space once the engine is loaded (Linux, `RLIMIT_AS`), in bytes.
    pub memory_growth: u64,
    /// Seconds of processor time before the operating system ends the worker (`RLIMIT_CPU`; Linux and macOS). 0: no limit.
    pub cpu_secs: u64,
    pub engine: super::EngineLimits,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ToWorker {
    /// Run a stored procedure or a trigger. The worker answers with any number of `Host` messages, then `Done` or `Failed`.
    Run { body: String, cfg: Value, args: Value, remaining_ms: u64, panic: bool },
    /// Call a user-defined function. `def` is the function's text the first time a function of that name is called. The worker
    /// answers `UdfDone` or `Failed`.
    Udf { name: String, def: Option<String>, args: Vec<Value>, undefined: Vec<usize>, remaining_ms: u64 },
    /// The answer to a `Host` message.
    Reply { text: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum FromWorker {
    /// A collection call (or a log line) of the script.
    Host {
        op: String,
        payload: String,
    },
    Done {
        outcome: Outcome,
    },
    UdfDone {
        text: String,
    },
    Failed {
        failure: Failure,
    },
}

/// Why a message could not be read.
#[derive(Debug)]
pub enum FrameError {
    /// The other side closed the pipe (or the read failed): it ended.
    Closed,
    /// The length in front of the message is more than the receiver allows.
    TooLarge(usize),
    /// The bytes are not a message of this language.
    Garbage(String),
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(body.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "message too large"))?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// Reads one message of at most `max` bytes. The length is checked before anything is allocated, and the memory grows with
/// what really arrives, so a length that is not followed by that many bytes costs nothing.
pub fn read_frame<R: Read, T: for<'a> Deserialize<'a>>(r: &mut R, max: usize) -> Result<T, FrameError> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head).map_err(|_| FrameError::Closed)?;
    let len = u32::from_be_bytes(head) as usize;
    if len > max {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = Vec::with_capacity(len.min(64 * 1024));
    let got = r.take(len as u64).read_to_end(&mut buf).map_err(|_| FrameError::Closed)?;
    if got < len {
        return Err(FrameError::Closed);
    }
    serde_json::from_slice(&buf).map_err(|e| FrameError::Garbage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_are_bounded() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &FromWorker::Host { op: "read".into(), payload: "{}".into() }).unwrap();
        let m: FromWorker = read_frame(&mut buf.as_slice(), 1024).unwrap();
        assert!(matches!(m, FromWorker::Host { ref op, .. } if op == "read"));
        // too large is refused before reading the body
        assert!(matches!(read_frame::<_, FromWorker>(&mut buf.as_slice(), 4), Err(FrameError::TooLarge(_))));
        // a length with nothing behind it is a closed pipe, and a short body too
        assert!(matches!(read_frame::<_, FromWorker>(&mut &[0u8, 0, 0, 9][..], 1024), Err(FrameError::Closed)));
        assert!(matches!(read_frame::<_, FromWorker>(&mut &[0u8, 0, 0, 9, 1, 2][..], 1024), Err(FrameError::Closed)));
        // garbage of the right length is garbage
        assert!(matches!(read_frame::<_, FromWorker>(&mut &[0u8, 0, 0, 2, b'x', b'y'][..], 1024), Err(FrameError::Garbage(_))));
        // a huge claimed length does not allocate it
        assert!(matches!(read_frame::<_, FromWorker>(&mut &[0x7f, 0xff, 0xff, 0xff][..], usize::MAX), Err(FrameError::Closed)));
    }
}
