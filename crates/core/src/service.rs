//! The traits a service crate implements.

use async_trait::async_trait;

use crate::{Request, Response};

/// How a service is reached on the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mount {
    /// `{account}.{label}.<anything>`
    AccountHost(&'static str),
    /// First host label equals this; account "".
    Host(&'static str),
    /// Path starts with this (not stripped); account "".
    PathPrefix(&'static str),
}

#[async_trait]
pub trait Service: Send + Sync + 'static {
    /// Unique lowercase name: "blob", "queue", "keyvault". Used by --pin and logs.
    fn name(&self) -> &'static str;
    fn mounts(&self) -> Vec<Mount>;
    /// On a pinned listener, take the account from the first path segment
    /// (Azurite style: `http://127.0.0.1:10000/devstoreaccount1/container`).
    fn path_style_on_pinned(&self) -> bool {
        false
    }
    /// Account used on a pinned listener when the request names none.
    fn default_account(&self) -> &'static str {
        "default"
    }
    /// Largest request body this service accepts for a request line. `None` (the default) means core's own
    /// limit (`EmulatorBuilder::max_body_bytes`, 1 GiB). Core refuses a bigger body with
    /// [`body_too_large`](Service::body_too_large) *before* reading it, so a service that never needs more than
    /// a megabyte does not have a gigabyte buffered first. The answer is capped by core's limit, so a service
    /// can only lower it. `path` is the raw path as the service will see it (account segment removed).
    fn max_body_bytes(&self, _method: &http::Method, _path: &str) -> Option<usize> {
        None
    }
    /// The answer for a body over the limit (`limit` is the limit that was in force). `None` (the default)
    /// is core's JSON `413 RequestBodyTooLarge`; a service returns its own error shape here.
    fn body_too_large(&self, _limit: usize) -> Option<Response> {
        None
    }
    /// Should core drop the handler of this request when the client goes away before the answer is complete?
    /// `false` (the default): the handler runs to its end whatever the client does, and its changes happen; this is
    /// the safe choice for a handler that changes state in several steps (Azure also finishes a request the client
    /// abandoned). `true` is for work that costs a lot and changes nothing outside the request: a query over a big
    /// blob, a script run. The request's [`CancelToken`](crate::CancelToken) is available to every handler either
    /// way; it is cancelled on disconnect only for a request this method said `true` for (and when the client drops
    /// a streamed answer before its end).
    fn abort_on_disconnect(&self, _method: &http::Method, _path: &str) -> bool {
        false
    }
    async fn handle(&self, req: Request) -> Response;
    /// Drop all state. Called by `POST /_azemu/reset`.
    fn reset(&self);
    /// Drop the state of one account (storage account, vault, store, namespace; whatever this service's
    /// `Request::account` names). Return `true` when it was done, also when the service has nothing for that
    /// account. The default returns `false`, meaning "not supported": `POST /_azemu/reset?account=<name>` then
    /// names this service in its answer.
    fn reset_account(&self, _account: &str) -> bool {
        false
    }

    // -- persistence (`--data-dir`); helpers and the file format are in `crate::persist` --------------

    /// `None` (the default): this service keeps nothing across restarts. `Some(n)`: it does, and `n`
    /// changes whenever the state that [`save`](Service::save) writes has changed (a counter raised on
    /// every change is enough; a reset is a change). Core saves a service only when the number differs
    /// from the one it saw at the last save. Must be cheap: it is polled.
    fn persist_generation(&self) -> Option<u64> {
        None
    }

    /// Write the state into `dir`, a directory that belongs to this service alone and exists. Called on
    /// a blocking thread, never on the async runtime, and never twice at once for one service. Do not
    /// hold the service's lock while encoding or writing: take what is needed under the lock (cheap
    /// clones of `Arc`s and `Bytes`), release it, then write. Replace files as a whole
    /// (`persist::save_file` / `persist::write_atomic`), so a crash during a save leaves the previous
    /// save readable, and remove files that no longer belong to the state.
    fn save(&self, _dir: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }

    /// Read back what [`save`](Service::save) wrote. Called once at start, before any listener opens,
    /// on a service that is still empty. A directory with no files means "start empty". An error
    /// stops the emulator from starting, so its message must name the file and say why.
    fn load(&self, _dir: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }
}

/// A byte stream: a plain TCP socket or a TLS session on top of one.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// A connection handed to a [`RawProtocol`]. The bytes that were sniffed are
/// still unread on `io`. The connection can be a plain socket, a TLS session, or a
/// WebSocket (AMQP over WebSocket) seen as a byte stream.
pub struct RawConn {
    pub io: Box<dyn Io>,
    pub peer: std::net::SocketAddr,
    pub local: std::net::SocketAddr,
    /// True when the client opened the connection with TLS (`amqps://`).
    pub tls: bool,
    /// The TLS server name the client asked for, if any. For a connection that arrived inside a
    /// WebSocket without a TLS server name, the host part of the HTTP `Host` header.
    pub server_name: Option<String>,
}

/// For protocols that are not HTTP (AMQP). The handler owns the connection.
/// Works on every listener, with and without TLS.
#[async_trait]
pub trait RawProtocol: Send + Sync + 'static {
    /// Called with the first bytes of a connection (at most 16, at least 4 unless
    /// the peer sent less). Return true to take the connection.
    fn sniff(&self, first_bytes: &[u8]) -> bool;
    async fn serve(&self, conn: RawConn);
}
