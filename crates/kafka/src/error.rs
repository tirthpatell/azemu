//! Decoding errors. Any of them means the client sent something that is not Kafka: the connection is closed.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The frame ended before the field did.
    Truncated,
    /// A value that cannot be right (a negative length, a count larger than the bytes left, bad UTF-8, ...).
    Invalid(&'static str),
    /// A size over a limit.
    TooLarge(&'static str),
    /// An API key or version the server does not serve.
    Unsupported { api_key: i16, version: i16 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => write!(f, "frame ended early"),
            Error::Invalid(m) => write!(f, "invalid: {m}"),
            Error::TooLarge(m) => write!(f, "too large: {m}"),
            Error::Unsupported { api_key, version } => write!(f, "unsupported api key {api_key} version {version}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
