//! AMQP 1.0, server side: type codec, frames, and the connection / session / link state machines.
//!
//! Layers, bottom to top:
//! * [`types`] and [`codec`]: the type system and its wire encodings.
//! * [`performatives`], [`terminus`], [`sasl`], [`error`], [`message`]: the protocol's composite types.
//! * [`frame`]: frame header, SASL and AMQP frame bodies, splitting of large transfers.
//!
//! No Service Bus knowledge lives here; Event Hubs can reuse the whole crate.

pub mod codec;
pub mod error;
pub mod fields;
pub mod frame;
pub mod message;
pub mod performatives;
pub mod sasl;
pub mod terminus;
pub mod types;

pub use error::{condition, Error};
pub use types::Value;
pub mod api;
pub mod client;
mod connection;
mod link;
pub mod reply;
mod session;
mod state;

pub use connection::serve_connection;
