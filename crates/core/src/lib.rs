//! `azemu-core`: server, routing, TLS and helpers shared by every azemu service.
//!

mod access;
mod answer;
mod cli;
mod persistence;
mod private_dir;
mod refusals;
mod request;
mod routing;
mod server;
mod service;
mod tls;
mod websocket;

pub mod cancel;
pub mod conditions;
pub mod etag;
pub mod json_error;
pub mod persist;
pub mod redact;
pub mod resp;
pub mod settings;
pub mod storage;
pub mod stream;
pub mod time;
pub mod unsupported;

#[cfg(feature = "testing")]
pub mod testing;

pub use async_trait::async_trait;
pub use {bytes, http, serde, serde_json};

pub use cancel::{CancelGuard, CancelToken};
pub use cli::{health_check_if_requested, init_logging_with, run_cli};
pub use persistence::{SaveOutcome, ServiceSave};
pub use request::{Endpoint, Query, Request, RequestCtx};
pub use server::{Emulator, EmulatorBuilder, RunningEmulator, DEFAULT_MAX_BODY_BYTES};
pub use service::{Io, Mount, RawConn, RawProtocol, Service};
pub use websocket::{WebSocketProtocol, WsAccept, WsConn, WsRequest, WsStream, DEFAULT_MAX_FRAME_BYTES};

/// The version of this build, for example `0.1.0`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The git commit this build was made from (12 characters), or `unknown` when there was no repository or no commit.
pub const GIT_COMMIT: &str = env!("AZEMU_GIT_COMMIT");
/// What `azemu --version` prints after the program name.
pub const BUILD_VERSION_LINE: &str = concat!(env!("CARGO_PKG_VERSION"), " (commit ", env!("AZEMU_GIT_COMMIT"), ")");

pub type Response = http::Response<bytes::Bytes>;
