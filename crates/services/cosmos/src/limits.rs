//! Limits on request input, so that no request can overflow the stack or grow memory
//! without bound.
//!
//! * Nesting: Microsoft documents 128 levels of embedded objects / arrays per item
//!   (Service quotas and default limits, "Per-item limits"). The JSON parser (serde_json) stops
//!   at 127 levels, so that is the number everything uses, and a stored document can never be
//!   deeper than a document that could have been created with a POST.
//! * Request size: 2 MB (same page, "Per-request limits").
//! * SQL text: 512 KB (same page, "SQL query limits").

use serde_json::Value;

use crate::errors::{CosmosError, R};

/// Deepest nesting of objects and arrays a document may have.
pub const MAX_NESTING: usize = 127;
/// Largest request body (2 MB).
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// Longest SQL query text (512 KB).
pub const MAX_QUERY_BYTES: usize = 512 * 1024;
/// Queries that run at the same time (each on a blocking thread, each allowed `QUERY_MEMORY_BYTES`). More
/// wait their turn without holding a thread: the worst case is this many queries times the memory budget.
pub const MAX_RUNNING_QUERIES: usize = 4;
/// What one query may hold at the same time (rows collected for ORDER BY, DISTINCT, groups, the result).
pub const QUERY_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Nesting depth of a value: a scalar is 0, `{}` and `[]` are 1. Iterative, so it is safe on
/// any value.
pub fn depth(v: &Value) -> usize {
    let mut max = 0;
    let mut stack: Vec<(&Value, usize)> = vec![(v, 0)];
    while let Some((v, d)) = stack.pop() {
        match v {
            Value::Array(a) => {
                max = max.max(d + 1);
                stack.extend(a.iter().map(|x| (x, d + 1)));
            }
            Value::Object(m) => {
                max = max.max(d + 1);
                stack.extend(m.values().map(|x| (x, d + 1)));
            }
            _ => {}
        }
    }
    max
}

/// Same as [`depth`] for the members of an object (the object itself is level 1).
pub fn check_map_nesting(m: &serde_json::Map<String, Value>) -> R<()> {
    let deepest = m.values().map(depth).max().unwrap_or(0);
    if deepest + 1 > MAX_NESTING {
        return Err(nesting_error());
    }
    Ok(())
}

pub fn nesting_error() -> CosmosError {
    CosmosError::bad_request(format!("The document is nested too deeply: objects and arrays can be nested at most {MAX_NESTING} levels."))
}

pub fn check_nesting(v: &Value) -> R<()> {
    if depth(v) > MAX_NESTING {
        return Err(nesting_error());
    }
    Ok(())
}

struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Length of the JSON text of a value, counted without building the text.
pub fn json_size(v: &Value) -> usize {
    let mut c = Counter(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}
