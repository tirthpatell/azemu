//! Frame encoding and decoding (part 2, section 2.3).
//!
//! ```text
//! +0       +4   +5    +6       +8
//! | SIZE   |DOFF| TYPE | CHANNEL | extended header | frame body |
//! ```

use bytes::{Buf, Bytes, BytesMut};

use crate::codec::{decode, encode, DecodeError};
use crate::performatives::Performative;
use crate::sasl::Sasl;

pub const FRAME_TYPE_AMQP: u8 = 0;
pub const FRAME_TYPE_SASL: u8 = 1;
/// The smallest max-frame-size a peer may use (spec 2.7.1).
pub const MIN_MAX_FRAME_SIZE: u32 = 512;

pub const HEADER_AMQP: [u8; 8] = *b"AMQP\x00\x01\x00\x00";
pub const HEADER_SASL: [u8; 8] = *b"AMQP\x03\x01\x00\x00";
pub const HEADER_TLS: [u8; 8] = *b"AMQP\x02\x01\x00\x00";

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum FrameBody {
    /// An empty frame: a heartbeat.
    Empty,
    /// A performative; `payload` is the message data of a transfer (empty for the rest).
    Amqp(Performative, Bytes),
    Sasl(Sasl),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub channel: u16,
    pub body: FrameBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Frame header is malformed or the size exceeds the limit: framing-error.
    Framing(String),
    /// The body does not decode: decode-error.
    Decode(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Framing(s) => write!(f, "framing error: {s}"),
            FrameError::Decode(s) => write!(f, "decode error: {s}"),
        }
    }
}
impl std::error::Error for FrameError {}

impl From<DecodeError> for FrameError {
    fn from(e: DecodeError) -> Self {
        FrameError::Decode(e.to_string())
    }
}

/// Try to take one frame off the front of `buf`. Returns `Ok(None)` when more bytes are needed.
/// `max_frame_size` bounds the size field before anything is buffered or allocated.
pub fn decode_frame(buf: &mut BytesMut, max_frame_size: u32) -> Result<Option<Frame>, FrameError> {
    if buf.len() < 8 {
        return Ok(None);
    }
    let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let doff = buf[4];
    let ftype = buf[5];
    let channel = u16::from_be_bytes([buf[6], buf[7]]);
    if size < 8 {
        return Err(FrameError::Framing(format!("frame size {size} is smaller than the 8 byte header")));
    }
    if size > max_frame_size {
        return Err(FrameError::Framing(format!("frame size {size} exceeds max-frame-size {max_frame_size}")));
    }
    if doff < 2 || (doff as u32) * 4 > size {
        return Err(FrameError::Framing(format!("invalid data offset {doff}")));
    }
    if buf.len() < size as usize {
        return Ok(None);
    }
    let mut frame = buf.split_to(size as usize);
    frame.advance(doff as usize * 4);
    let body = frame.freeze();
    let parsed = if body.is_empty() {
        FrameBody::Empty
    } else {
        match ftype {
            FRAME_TYPE_AMQP => {
                let (v, used) = decode(&body)?;
                let perf = Performative::from_value(&v)?;
                let payload = if matches!(perf, Performative::Transfer(_)) { body.slice(used..) } else { Bytes::new() };
                FrameBody::Amqp(perf, payload)
            }
            FRAME_TYPE_SASL => {
                let (v, _) = decode(&body)?;
                FrameBody::Sasl(Sasl::from_value(&v)?)
            }
            other => return Err(FrameError::Framing(format!("unknown frame type {other}"))),
        }
    };
    Ok(Some(Frame { channel, body: parsed }))
}

/// Append the wire bytes of `frame` to `out`.
pub fn encode_frame(frame: &Frame, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0, 2, 0]);
    out.extend_from_slice(&frame.channel.to_be_bytes());
    match &frame.body {
        FrameBody::Empty => {}
        FrameBody::Amqp(p, payload) => {
            encode(&p.to_value(), out);
            out.extend_from_slice(payload);
        }
        FrameBody::Sasl(s) => {
            out[start + 5] = FRAME_TYPE_SASL;
            encode(&s.to_value(), out);
        }
    }
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

pub fn frame_bytes(frame: &Frame) -> Vec<u8> {
    let mut v = Vec::new();
    encode_frame(frame, &mut v);
    v
}

/// Number of bytes a transfer frame spends before its payload: header + encoded performative.
pub fn transfer_overhead(t: &crate::performatives::Transfer) -> usize {
    8 + crate::codec::encode_to_vec(&Performative::Transfer(t.clone()).to_value()).len()
}

/// Split one delivery into transfer frames no larger than `max_frame_size`. The first frame
/// carries the full transfer fields; the rest carry just the handle, `more` and the settled flag
/// (no delivery-id: peers take a delivery-id on a continuation as a new delivery). `more` is set on all but the last.
pub fn split_transfer(channel: u16, first: crate::performatives::Transfer, payload: &[u8], max_frame_size: u32) -> Vec<Frame> {
    let max = max_frame_size.max(MIN_MAX_FRAME_SIZE) as usize;
    let mut frames = Vec::new();
    let mut offset = 0usize;
    let mut cur = first;
    loop {
        let overhead = transfer_overhead(&cur) + 1;
        let room = max.saturating_sub(overhead).max(1);
        let take = room.min(payload.len() - offset);
        let last = offset + take >= payload.len();
        cur.more = !last;
        frames.push(Frame {
            channel,
            body: FrameBody::Amqp(Performative::Transfer(cur.clone()), Bytes::copy_from_slice(&payload[offset..offset + take])),
        });
        offset += take;
        if last {
            break;
        }
        cur = crate::performatives::Transfer { handle: cur.handle, settled: cur.settled, ..Default::default() };
    }
    frames
}
