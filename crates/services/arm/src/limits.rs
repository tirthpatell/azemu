//! Limits for ARM templates, and the bookkeeping that keeps one request from using unbounded time or memory.
//!
//! Numbers from Microsoft Learn ("ARM template best practices", "Azure subscription limits", "Template functions"):
//! a template expression has at most 24,576 characters; at most 256 parameters, 512 variables, 800 resources
//! (copy count included) and 64 outputs; `range` makes at most 10,000 numbers and `start + count` stays within
//! 2,147,483,647; a `copy` count is at most 800 for resources. The documentation names no limit for how deep
//! functions nest, how long evaluation may take or how deep nested deployments go, so those are azemu's own choices
//! (they are generous for a real template and small enough to stay inside a 2 MiB thread stack).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::Value;

pub const MAX_EXPRESSION_CHARS: usize = 24_576;
pub const MAX_PARAMETERS: usize = 256;
pub const MAX_VARIABLES: usize = 512;
pub const MAX_RESOURCES: usize = 800;
pub const MAX_OUTPUTS: usize = 64;
pub const MAX_RANGE_COUNT: i64 = 10_000;
pub const MAX_RANGE_END: i64 = 2_147_483_647;
pub const MAX_COPY_RESOURCES: i64 = 800;
/// `copy` in properties, variables and outputs.
pub const MAX_COPY_PROPERTIES: i64 = 10_000;

/// azemu's own limits (see the module comment).
/// How many calls and index operations may be open inside one another in one expression.
pub const MAX_PARSE_DEPTH: usize = 64;
/// Start of the parser's message for an expression that nests too deeply (the evaluator turns it into `EvalError::Invalid`).
pub const TOO_DEEP: &str = "the expression nests functions and indexes more than";
/// How many evaluation steps (a call, a JSON level, a variable reference) may be open inside one another.
pub const MAX_EVAL_DEPTH: usize = 128;
/// Largest single value an expression may make (string bytes, plus 16 per JSON node).
pub const MAX_VALUE_BYTES: u64 = 4 * 1024 * 1024;
/// A value may nest at most this deep, so that clone, drop and printing never recurse far.
pub const MAX_VALUE_DEPTH: usize = 100;
/// Work one request may do evaluating templates (bytes touched, plus a flat charge per call).
pub const MAX_BUDGET: u64 = 64 * 1024 * 1024;
/// What one `reference()` costs: it scans the stored resources.
pub const REFERENCE_COST: u64 = 16 * 1024;
/// Evaluated variables kept for reuse, in bytes.
pub const MAX_MEMO_BYTES: u64 = 32 * 1024 * 1024;
/// Deployments inside deployments.
pub const MAX_NESTED_DEPLOYMENTS: usize = 10;
/// Resources of all the (nested) deployments of one request.
pub const MAX_TOTAL_RESOURCES: usize = 10_000;
/// Sum of the sizes of the evaluated resource bodies of one request (Azure: 4 MB after expansion).
pub const MAX_EXPANDED_BYTES: u64 = 4 * 1024 * 1024;
/// Widest number a `format` specifier may ask for (`{0:D4000000000}`).
pub const MAX_FORMAT_WIDTH: usize = 1024;

/// Work and size counters of one request, shared by every template (nested deployments too) it runs.
#[derive(Default)]
pub struct Budget {
    used: AtomicU64,
    pub resources: AtomicUsize,
    pub expanded_bytes: AtomicU64,
}

impl Budget {
    /// Count `units` of work; `Err` with a message once the request has used its share.
    pub fn charge(&self, units: u64) -> Result<(), String> {
        let total = self.used.fetch_add(units, Ordering::Relaxed).saturating_add(units);
        if total > MAX_BUDGET {
            Err(format!(
                "The template is too complex to evaluate: it needs more than {} MB of work (the limit is a safeguard of the emulator; reuse values through variables and keep copy loops small).",
                MAX_BUDGET / (1024 * 1024)
            ))
        } else {
            Ok(())
        }
    }
}

/// Variables of one template that are already evaluated, and the ones being evaluated right now (to find cycles).
#[derive(Default)]
pub struct Memo {
    done: Mutex<HashMap<String, Value>>,
    bytes: AtomicU64,
    active: Mutex<HashSet<String>>,
}

impl Memo {
    pub fn get(&self, key: &str) -> Option<Value> {
        self.done.lock().get(key).cloned()
    }

    pub fn put(&self, key: String, v: &Value, weight: u64) {
        if self.bytes.load(Ordering::Relaxed).saturating_add(weight) <= MAX_MEMO_BYTES {
            self.bytes.fetch_add(weight, Ordering::Relaxed);
            self.done.lock().insert(key, v.clone());
        }
    }

    /// `false` when the variable is already being evaluated: it refers to itself.
    pub fn enter(&self, key: &str) -> bool {
        self.active.lock().insert(key.to_string())
    }

    pub fn leave(&self, key: &str) {
        self.active.lock().remove(key);
    }
}

/// Size of a value in bytes (strings) plus 16 per node; `Err` when it is bigger than `MAX_VALUE_BYTES` or nests deeper
/// than `MAX_VALUE_DEPTH`. Walks the value without recursion.
pub fn weight(v: &Value) -> Result<u64, String> {
    let mut total: u64 = 0;
    let mut stack: Vec<(&Value, usize)> = vec![(v, 1)];
    while let Some((x, depth)) = stack.pop() {
        if depth > MAX_VALUE_DEPTH {
            return Err(format!("The value is nested more than {MAX_VALUE_DEPTH} levels deep."));
        }
        total += 16;
        match x {
            Value::String(t) => total += t.len() as u64,
            Value::Array(a) => stack.extend(a.iter().map(|e| (e, depth + 1))),
            Value::Object(o) => {
                for (k, e) in o {
                    total += k.len() as u64;
                    stack.push((e, depth + 1));
                }
            }
            _ => {}
        }
        if total > MAX_VALUE_BYTES {
            return Err(format!("The value is larger than {} MB.", MAX_VALUE_BYTES / (1024 * 1024)));
        }
    }
    Ok(total)
}

/// A copy of the value in which strings are lower case and objects are sorted, as text: two values that
/// `eq_values` calls equal give the same text. Lets `union` and `distinct`-style work run in linear time.
pub fn canonical(v: &Value) -> String {
    fn go(v: &Value, out: &mut String) {
        match v {
            Value::String(t) => {
                out.push('"');
                out.push_str(&t.to_ascii_lowercase().replace('\\', "\\\\").replace('"', "\\\""));
                out.push('"');
            }
            Value::Array(a) => {
                out.push('[');
                for e in a {
                    go(e, out);
                    out.push(',');
                }
                out.push(']');
            }
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                out.push('{');
                for k in keys {
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    go(&o[k], out);
                    out.push(',');
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut s = String::new();
    go(v, &mut s);
    s
}
