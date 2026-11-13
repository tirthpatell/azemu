//! The Apache Kafka wire protocol as a library: byte readers and writers, request and response headers, the message
//! layouts of the APIs the emulator serves (as data, generated from Kafka's own definitions, interpreted by one codec)
//! and the record batch format v2. No I/O and no C code.
//!
//! The rule of the AMQP library applies here too: a length or a count read from the wire never sizes an
//! allocation before the bytes are there (a count must fit in the bytes that are left, a decoded request may hold at
//! most as many values as the request has bytes, strings and byte arrays are copied only after their length was
//! checked against what is left), nesting is bounded by the schema, and a frame has a size limit. Anything wrong is an
//! `Err`; the server closes the connection.

pub mod api;
pub mod crc32c;
pub mod error;
pub mod header;
pub mod records;
pub mod schema;
mod schemas;
pub mod value;
pub mod wire;

pub use error::{Error, Result};
pub use header::{peek_header, read_request, read_response, write_request, write_response, Request, RequestHeader};
pub use value::{Struct, Value};
