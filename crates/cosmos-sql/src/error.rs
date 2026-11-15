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
    /// More errors the service reports in the same answer (it lists every problem it finds in a query).
    pub more: Vec<Detail>,
    /// How the answer is written (see [`Shape`]).
    pub shape: Shape,
    /// The sub-status the service puts in the response header, if it has one.
    pub substatus: Option<u32>,
}

/// One more error in the list of an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    /// Cosmos error code, for example `SC2201`.
    pub code: String,
    /// Human readable message.
    pub message: String,
    /// Start of the offending text, in characters.
    pub start: usize,
    /// End of the offending text (exclusive), in characters.
    pub end: usize,
}

/// The form of the error text of a `400`. Azure writes the same kind of problem in four ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `{"errors":[{"severity":"Error","location":{..},"code":"SC2201","message":".."}]}`: the SQL compiler.
    Located,
    /// `{"errors":[{"severity":"Error","code":4001,"message":".."}]}`: a number as the code and no location.
    Coded,
    /// `{"Errors":["message"]}`: the gateway, no code.
    Gateway,
    /// The message as it is, not JSON.
    Text,
}

impl SqlError {
    pub(crate) fn new(kind: ErrorKind, code: &str, message: String, start: usize, end: usize) -> Self {
        SqlError { kind, code: code.to_string(), message, start, end, more: Vec::new(), shape: Shape::Located, substatus: None }
    }

    /// An error the gateway answers with `{"Errors":["message"]}` (no code, no position).
    pub fn gateway(message: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::Semantic, "", message.into(), 0, 0);
        e.shape = Shape::Gateway;
        e
    }

    /// An error written as `{"errors":[{"severity":"Error","code":<number>,"message":".."}]}`.
    pub fn coded(code: u32, message: impl Into<String>, substatus: Option<u32>) -> Self {
        let mut e = Self::new(ErrorKind::Evaluation, &code.to_string(), message.into(), 0, 0);
        e.shape = Shape::Coded;
        e.substatus = substatus;
        e
    }

    /// An error whose message is the whole text of the answer.
    pub fn text(message: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::Semantic, "", message.into(), 0, 0);
        e.shape = Shape::Text;
        e
    }

    /// Adds another error to the list of this answer.
    pub(crate) fn and(mut self, code: &str, message: String, start: usize, end: usize) -> Self {
        self.more.push(Detail { code: code.to_string(), message, start, end });
        self
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

    /// The same error with another code.
    pub(crate) fn with_code(mut self, code: &str) -> Self {
        self.code = code.to_string();
        self
    }

    /// An error with a position (the code is the compiler's, `SC....`).
    pub(crate) fn at(code: &str, message: String, start: usize, end: usize) -> Self {
        let kind = if code.starts_with("SC1") { ErrorKind::Syntax } else { ErrorKind::Semantic };
        Self::new(kind, code, message, start, end)
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
        match self.shape {
            Shape::Gateway => serde_json::json!({"Errors":[self.message]}),
            Shape::Text => Value::String(self.message.clone()),
            Shape::Coded => serde_json::json!({"errors":[{
                "severity": "Error",
                "code": self.code.parse::<u64>().map(Value::from).unwrap_or_else(|_| Value::from(self.code.as_str())),
                "message": self.message,
            }]}),
            Shape::Located => {
                let one = |code: &str, message: &str, start: usize, end: usize| {
                    serde_json::json!({
                        "severity": "Error",
                        "location": {"start": start, "end": end},
                        "code": code,
                        "message": message,
                    })
                };
                let mut list = vec![one(&self.code, &self.message, self.start, self.end)];
                list.extend(self.more.iter().map(|d| one(&d.code, &d.message, d.start, d.end)));
                serde_json::json!({ "errors": list })
            }
        }
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}..{}): {}", self.code, self.start, self.end, self.message)
    }
}

impl std::error::Error for SqlError {}
