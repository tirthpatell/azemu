//! Error types.

use std::fmt;

/// The filter or action text is not valid. `position` is a 0-based
/// character index into the input.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl ParseError {
    pub(crate) fn new(message: impl Into<String>, position: usize) -> Self {
        ParseError { message: message.into(), position }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at position {})", self.message, self.position)
    }
}

impl std::error::Error for ParseError {}

/// An evaluation failure (Service Bus raises a `FilterException`): reading a
/// nonexistent system property, division by zero, an invalid `ESCAPE`, ...
/// A filter that fails to evaluate does not match; an action that fails
/// leaves the message unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalError {
    pub message: String,
}

impl EvalError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        EvalError { message: message.into() }
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EvalError {}
