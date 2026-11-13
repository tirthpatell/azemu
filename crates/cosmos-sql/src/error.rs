//! Error type shared by the parser and the evaluator.

use serde_json::Value;

/// Broad class of an error, so a service can pick an HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The query text is not valid (`SC1001`).
    Syntax,
    /// The query parses but is not valid (unknown identifier or function, bad arguments).
    Semantic,
    /// A valid feature that this crate does not implement (UDFs, `ST_*`, vector, full text).
    Unsupported,
    /// A runtime problem (missing parameter, bad `TOP` value). Cosmos answers 400 for these too.
    Evaluation,
    /// A safety limit was hit (nesting depth, number of joined rows).
    LimitExceeded,
}

/// Error produced by parsing or evaluating a query.
///
/// All kinds map to `400 BadRequest` on real Cosmos DB. Use
/// [`SqlError::to_cosmos_json`] for the response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    /// Class of the error.
    pub kind: ErrorKind,
    /// Cosmos error code, for example `SC1001` for a syntax error.
    pub code: String,
    /// Human readable message.
    pub message: String,
    /// Start of the offending text, in characters from the start of the query.
    pub start: usize,
    /// End of the offending text (exclusive), in characters.
    pub end: usize,
}

impl SqlError {
    pub(crate) fn new(kind: ErrorKind, code: &str, message: String, start: usize, end: usize) -> Self {
        SqlError { kind, code: code.to_string(), message, start, end }
    }

    pub(crate) fn syntax_near(text: &str, start: usize, end: usize) -> Self {
        Self::new(ErrorKind::Syntax, "SC1001", format!("Syntax error, incorrect syntax near '{text}'."), start, end)
    }

    pub(crate) fn syntax(message: &str, start: usize, end: usize) -> Self {
        Self::new(ErrorKind::Syntax, "SC1001", message.to_string(), start, end)
    }

    pub(crate) fn semantic(code: &str, message: String, start: usize, end: usize) -> Self {
        Self::new(ErrorKind::Semantic, code, message, start, end)
    }

    /// Error for features this crate does not implement.
    pub fn not_supported(what: &str, start: usize, end: usize) -> Self {
        Self::new(ErrorKind::Unsupported, "SC9999", format!("{what} is not supported."), start, end)
    }

    pub(crate) fn eval(message: String) -> Self {
        Self::new(ErrorKind::Evaluation, "SC2100", message, 0, 0)
    }

    /// A user-defined function failed (its script threw, timed out, or the host cannot run it).
    pub fn udf(message: String) -> Self {
        Self::new(ErrorKind::Evaluation, "SC2103", message, 0, 0)
    }

    pub(crate) fn limit(message: String) -> Self {
        Self::new(ErrorKind::LimitExceeded, "SC2101", message, 0, 0)
    }

    /// `{"errors":[{"severity":"Error","location":{"start":..,"end":..},"code":..,"message":..}]}`
    ///
    /// This is the shape of the `message` of a Cosmos `400` error body for a bad query.
    pub fn to_cosmos_json(&self) -> Value {
        serde_json::json!({"errors":[{
            "severity": "Error",
            "location": {"start": self.start, "end": self.end},
            "code": self.code,
            "message": self.message,
        }]})
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}..{}): {}", self.code, self.start, self.end, self.message)
    }
}

impl std::error::Error for SqlError {}
