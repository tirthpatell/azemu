# AMQP over WebSocket

**What it emulates.** AMQP 1.0 carried inside WebSocket frames, for Service Bus and Event Hubs clients (`TransportType.AmqpOverWebsocket` in Python, the default in
browsers, the usual choice behind proxies that pass only 443). Core accepts the upgrade on every listener, plain (`ws://`) and inside TLS (`wss://`), and hands the
byte stream to the AMQP handler, so Service Bus and Event Hubs get it without code of their own.

**How to reach it.** The same endpoints as plain AMQP (`docs/services/servicebus.md`, `docs/services/eventhubs.md`); the client is told to use WebSocket, and for most
SDKs it must also be given the custom endpoint (see "How each SDK is pointed at it").

**Not built.** Compression (`permessage-deflate`), pings from the server, HTTP/2 WebSocket (RFC 8441), a check of the path (real Azure uses `/$servicebus/websocket`; any
path is accepted here) and a check of the subprotocol beyond `amqp`, `AMQPWSB10` or none. Plain `ws://` works, which Azure does not serve.

## What it is

Service Bus and Event Hubs clients can carry AMQP inside WebSocket (`TransportType.AmqpOverWebsocket` in Python, the
default in browsers, the usual choice behind proxies that only pass 443). The client sends an HTTP/1.1 request with
`Upgrade: websocket`, gets `101 Switching Protocols`, and from then on every WebSocket binary frame carries AMQP bytes.
Frame boundaries do not line up with AMQP frame boundaries: the payloads are one byte stream.

azemu accepts this on its one port, on every listener (gateway or pinned), with and without TLS: `ws://` and `wss://`.
(Real Azure serves WebSocket only over TLS on 443; the emulator is more lenient.)

## How it works

```
client                       core (server.rs)                          service
  GET /… Upgrade: websocket  ─►  try_websocket: is it an upgrade request?
  Sec-WebSocket-Protocol: …      a registered WebSocketProtocol wants it?  no → the raw-protocol fallback wants it?
                             ◄─  101, Sec-WebSocket-Accept, chosen subprotocol
  binary frames (masked)     ─►  WsStream: unmask, join fragments, answer ping/close  ─►  bytes
                             ◄─  WsStream: unmasked binary frames of at most 32 KiB    ◄─  bytes
```

- **The contract** (`WebSocketProtocol`, `WsRequest`, `WsAccept`, `WsConn`) is general and does not know AMQP:
  `accept(&WsRequest) -> Option<WsAccept>` says whether the handler wants the request (and which offered subprotocol to
  answer with), `serve(WsConn)` gets the connection as a byte stream (`WsConn::io`, a `WsStream`) plus the request
  (path, query, `Host`, headers, TLS flag, peer, server name). Register with `EmulatorBuilder::websocket_protocol`.
  A request that no handler wants is ordinary HTTP (usually a 404 from the gateway).
- **The fallback for raw protocols** (`RawOverWebSocket`): when no registered handler wants the request, core takes it
  if some raw protocol is registered and the client offers the subprotocol `amqp` or `AMQPWSB10` (or none). After the
  `101` it reads the first bytes through the stream and gives the connection to the raw protocol whose `sniff` matches,
  exactly as for a plain TCP connection. If none matches the server closes the WebSocket with status 1003. Service
  crates and `crates/azemu/src/main.rs` need no change: AMQP over WebSocket ends up in the same `AmqpMux` /
  Service Bus / Event Hubs code, in the unified binary and in `azemu-servicebus` and `azemu-eventhubs`.
- **Which service**: still the `hostname` of the AMQP `open` frame. The Python SDK sends the namespace host there
  (`default.servicebus.localhost`, `myns.eventhub.localhost`) even though the HTTP `Host` is whatever the client dialled
  (for example `127.0.0.1:port`; a test checks that this still picks Event Hubs). `RawConn::server_name` is the TLS
  server name when there is one, otherwise the host part of the HTTP `Host` header.
- **Framing** (`WsStream`, RFC 6455 server side, written by hand; the only crypto is SHA-1 and base64 for the handshake):
  - Client frames must be masked, reserved bits must be zero, control frames must be at most 125 bytes and not
    fragmented, a continuation needs an open message, a new message cannot start inside an open one. A violation closes
    with 1002. Text frames are treated like binary frames (no UTF-8 check).
  - A payload is unmasked while it is read into the caller's buffer: nothing is buffered except one control frame
    (at most 125 bytes). A frame bigger than the limit (16 MiB by default, `websocket_max_frame_bytes`) closes with 1009.
  - Ping is answered with pong (only the latest ping if several pile up), even when nothing else is being written.
    Close is answered with close (same status code) and the read side ends. The peer dropping the TCP connection
    between frames is a normal end of stream; in the middle of a frame it is an error.
  - Writes become unmasked binary frames of at most 32 KiB. One frame is held at a time, so a slow client stalls the
    writer instead of growing memory. `shutdown` sends close 1000 and then ends the transport.
  - Not supported: extensions (`permessage-deflate` is not offered back, so a client sends uncompressed data),
    client-side use.
- **Handshake checks**: `GET`, HTTP/1.1, `Upgrade: websocket`, `Connection: upgrade`, `Sec-WebSocket-Version: 13`
  (otherwise `426` with `Sec-WebSocket-Version: 13`), `Sec-WebSocket-Key` = base64 of 16 bytes (otherwise `400`).
  The path is not checked: the Python SDKs ask for `//$servicebus/websocket` (a double slash, because they add a path to
  the one in the custom endpoint) or `/`; other SDKs use `/$servicebus/websocket`.
- Open WebSocket connections end when the emulator shuts down.

## How each SDK is pointed at it (Python)

Checked in `azure/servicebus/_pyamqp/_transport.py` and `azure/eventhub/_pyamqp/_transport.py` (same code in both) and the
async twins. Versions used: `azure-servicebus` 7.15.0, `azure-eventhub` 5.15.1, `websocket-client` 1.9.2 (sync),
`aiohttp` 3.14.5 (async). `.venv` has no `pip`; `websocket-client` was installed with
`uv pip install --python .venv/bin/python websocket-client`.

- Subprotocol: `Sec-WebSocket-Protocol: AMQPWSB10`. The server echoes it exactly (`websocket-client` rejects a handshake
  whose answer does not name an offered subprotocol).
- URL: `wss://<host>` (sync: `ws://` when `use_tls` is false, async the same). **The port is not taken from the
  connection string**: the transport sets port 443, and the sync client does not even put a port in the URL, so
  `ws://myns.eventhub.localhost` goes to port 80. The only way to reach another port is
  **`custom_endpoint_address`**: `http://<host>:<port>/` gives `ws://`, `https://<host>:<port>/` gives `wss://` (the
  scheme is ignored, `use_tls` decides; the path of the address is added to the request path).
- Plain `ws://`: the connection string with `UseDevelopmentEmulator=true` (that sets `use_tls=False`) plus
  `custom_endpoint_address="http://myns.servicebus.localhost:<port>/"` and `transport_type=TransportType.AmqpOverWebsocket`.
- `wss://`: no `UseDevelopmentEmulator`; `ServiceBusClient("myns.servicebus.localhost", credential, transport_type=…,
  custom_endpoint_address="https://myns.servicebus.localhost:<port>/", connection_verify="<ca.pem>")`.
  `connection_verify` works for both the sync client (`sslopt={"ca_certs": …}`) and the async client (`ssl.SSLContext`).
  Same for `EventHubProducerClient` / `EventHubConsumerClient` with `myns.eventhub.localhost`.
- The async client sends a WebSocket ping every 10 seconds and closes the connection if no pong comes back; the emulator
  answers (tested with a 13 second idle wait).
- Event Hubs: `EventHubConsumerClient.receive_batch` over WebSocket reads each frame with a 1 second timeout and hands a
  batch to the callback only when frames stop for that long. A steady stream of events (one every 0.3 s) keeps the batch
  open, so a callback can seem late. This is the SDK, not the emulator (`ehutil.Collector.wait_live` sends a marker every
  0.3 s, which is why the WebSocket tests do not use it).
- Other SDKs (JavaScript in the browser, .NET `ServiceBusTransportType.AmqpWebSockets`) were not tried. They send
  `/$servicebus/websocket` and the subprotocol `amqp` (.NET) or `AMQPWSB10`; both are accepted.

## Limits and known differences from Azure

- No compression (`permessage-deflate`), no ping from the server (the AMQP layer has its own idle timeout), no
  subprotocol check beyond "`amqp`, `AMQPWSB10` or none".
- Any path is accepted for the raw-protocol fallback; real Azure uses `/$servicebus/websocket`.
- Plain `ws://` is accepted; real Azure serves WebSocket only over TLS.
- A frame from the client is limited to 16 MiB (default; AMQP frames are far smaller: the SDKs use up to 1 MiB).
- A client that upgrades and then sends nothing for 5 seconds (the same wait as for plain connections) is closed with
  status 1003.
