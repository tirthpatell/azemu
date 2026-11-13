//! Structured messages (`x-ms-structured-body: XSM/1.0; properties=crc64`).
//!
//! A client that wants a checksum on every piece of a body wraps the data in a message: a header,
//! one or more segments (each with its own CRC-64) and a trailer with the CRC-64 of everything.
//! The service takes the message apart on upload (Put Blob, Put Block, Put Page, Append Block,
//! Data Lake append), checks every CRC and stores only the data; on a download that asks for the
//! header it wraps the data again. The layout is the one the Go and Python SDKs write
//! (all numbers little endian):
//!
//! ```text
//! message  = header, segment x N, trailer
//! header   = version u8 (1), message length u64, flags u16 (bit 0: CRC-64), segment count u16   (13 bytes)
//! segment  = number u16 (from 1), data length u64, data, CRC-64 of the data                      (10 + n + 8)
//! trailer  = CRC-64 of all the data                                                              (8)
//! ```
//!
//! The CRC is CRC-64/NVME, the same one `x-ms-content-crc64` uses.

use azemu_core::bytes::Bytes;
use azemu_core::{Request, Response};

use crate::errors::{err, ApiError};
use crate::util::crc64;

/// The only header value that exists.
pub const HEADER_VALUE: &str = "XSM/1.0; properties=crc64";
pub const HEADER: &str = "x-ms-structured-body";
pub const LENGTH_HEADER: &str = "x-ms-structured-content-length";

const HEADER_SIZE: usize = 13;
const SEGMENT_HEADER: usize = 10;
const CRC: usize = 8;
/// Segment size of the messages the service writes (the SDKs' default).
const SEGMENT: usize = 4 * 1024 * 1024;

/// A message that cannot be taken apart: what is wrong with it.
pub enum Fault {
    /// Layout is wrong (short, wrong version, wrong lengths).
    Malformed(String),
    /// A CRC-64 does not match the data.
    Crc(String),
}

/// Wrap `data` in a message with segments of at most `segment` bytes.
pub fn encode(data: &[u8], segment: usize) -> Vec<u8> {
    let segment = segment.max(1);
    let mut count = data.len().div_ceil(segment).max(1);
    let mut seg = segment;
    if count > u16::MAX as usize {
        // The count is a u16: grow the segments until it fits.
        seg = data.len().div_ceil(u16::MAX as usize);
        count = data.len().div_ceil(seg);
    }
    let total = HEADER_SIZE + count * (SEGMENT_HEADER + CRC) + data.len() + CRC;
    let mut out = Vec::with_capacity(total);
    out.push(1u8);
    out.extend_from_slice(&(total as u64).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let part = &data[(i * seg).min(data.len())..((i + 1) * seg).min(data.len())];
        out.extend_from_slice(&(i as u16 + 1).to_le_bytes());
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(&crc64(part).to_le_bytes());
    }
    out.extend_from_slice(&crc64(data).to_le_bytes());
    out
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// Take a message apart: check the layout and every CRC-64, return the data.
pub fn decode(msg: &[u8]) -> Result<Vec<u8>, Fault> {
    let bad = |s: String| Fault::Malformed(s);
    if msg.len() < HEADER_SIZE {
        return Err(bad(format!("the structured message is {} bytes, shorter than its {HEADER_SIZE} byte header", msg.len())));
    }
    if msg[0] != 1 {
        return Err(bad(format!("structured message version {} is not supported", msg[0])));
    }
    let declared = le_u64(&msg[1..9]);
    if declared != msg.len() as u64 {
        return Err(bad(format!("the structured message header says {declared} bytes, the body has {}", msg.len())));
    }
    let flags = le_u16(&msg[9..11]);
    if flags & 1 == 0 {
        return Err(bad(format!("the structured message does not have the CRC-64 flag (flags {flags:#06x})")));
    }
    let segments = le_u16(&msg[11..13]);
    let mut data = Vec::with_capacity(msg.len());
    let mut at = HEADER_SIZE;
    for want in 1..=segments {
        if msg.len() < at + SEGMENT_HEADER {
            return Err(bad(format!("segment {want}: the message ends inside the segment header")));
        }
        let number = le_u16(&msg[at..at + 2]);
        let len = le_u64(&msg[at + 2..at + 10]);
        at += SEGMENT_HEADER;
        if number != want {
            return Err(bad(format!("segment number {number} found where {want} was expected")));
        }
        let room = (msg.len() - at) as u64;
        if len > room || room - len < CRC as u64 {
            return Err(bad(format!("segment {want}: its length {len} does not fit in the message")));
        }
        let end = at + len as usize;
        let part = &msg[at..end];
        if le_u64(&msg[end..end + CRC]) != crc64(part) {
            return Err(Fault::Crc(format!("the CRC64 of segment {want} does not match its data")));
        }
        data.extend_from_slice(part);
        at = end + CRC;
    }
    if msg.len() < at + CRC {
        return Err(bad("the message ends before its trailer".to_string()));
    }
    if le_u64(&msg[at..at + CRC]) != crc64(&data) {
        return Err(Fault::Crc("the CRC64 in the message trailer does not match the data".to_string()));
    }
    at += CRC;
    if at != msg.len() {
        return Err(bad(format!("{} bytes follow the structured message trailer", msg.len() - at)));
    }
    Ok(data)
}

/// Whether the header value names the one message format that exists (case and spaces around
/// the `;` do not matter).
fn is_known(v: &str) -> bool {
    let norm: String = v.split(';').map(str::trim).collect::<Vec<_>>().join("; ");
    norm.eq_ignore_ascii_case(HEADER_VALUE)
}

/// The error for a header that asks for something the service does not know.
fn bad_header(name: &str, value: &str) -> ApiError {
    ApiError::invalid_header(name, value)
}

fn fault_error(f: Fault) -> ApiError {
    match f {
        Fault::Crc(m) => err("Crc64Mismatch").status(400).msg(m),
        Fault::Malformed(m) => err("InvalidInput").msg(format!("x-ms-structured-body: {m}.")),
    }
}

/// An upload that carries `x-ms-structured-body`: the same request with the data in place of
/// the message (the headers stay as sent: the signature covers `Content-Length`). `None` when the header is absent. Every CRC
/// is checked, and `x-ms-structured-content-length` must be the length of the data.
pub fn unwrap_upload(req: &Request) -> Result<Option<Request>, ApiError> {
    let Some(kind) = req.header(HEADER) else { return Ok(None) };
    if !is_known(kind) {
        return Err(bad_header(HEADER, kind));
    }
    let length = match req.header(LENGTH_HEADER) {
        None => return Err(ApiError::missing_header(LENGTH_HEADER)),
        Some(v) => v.trim().parse::<u64>().map_err(|_| bad_header(LENGTH_HEADER, v))?,
    };
    let data = decode(&req.body).map_err(fault_error)?;
    if data.len() as u64 != length {
        return Err(bad_header(LENGTH_HEADER, &length.to_string())
            .msg(format!("x-ms-structured-content-length is {length}, the structured message holds {} bytes.", data.len())));
    }
    let mut out = req.clone();
    out.body = Bytes::from(data);
    Ok(Some(out))
}

/// Mirror `x-ms-structured-body` in the answer to an upload that was a message.
pub fn echo(resp: &mut Response) {
    if resp.status().is_success() {
        resp.headers_mut().insert(HEADER, azemu_core::http::HeaderValue::from_static(HEADER_VALUE));
    }
}

/// Check the header of a download. `Ok(true)`: the answer is to be wrapped (`wrap_download`).
pub fn download_wanted(req: &Request) -> Result<bool, ApiError> {
    match req.header(HEADER) {
        None => Ok(false),
        Some(v) if is_known(v) => Ok(true),
        Some(v) => Err(bad_header(HEADER, v)),
    }
}

/// Wrap the body of a successful Get Blob answer in a message and say so in the headers.
/// `Content-MD5` and `x-ms-content-crc64` describe the data, not the message, and are dropped.
pub fn wrap_download(resp: &mut Response) {
    if !matches!(resp.status().as_u16(), 200 | 206) {
        return;
    }
    let data = resp.body().clone();
    let msg = encode(&data, SEGMENT);
    *resp.body_mut() = Bytes::from(msg);
    let h = resp.headers_mut();
    h.remove("content-md5");
    h.remove("x-ms-content-crc64");
    h.insert(HEADER, azemu_core::http::HeaderValue::from_static(HEADER_VALUE));
    if let Ok(v) = azemu_core::http::HeaderValue::from_str(&data.len().to_string()) {
        h.insert(LENGTH_HEADER, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_several_segments() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 253) as u8).collect();
        for seg in [1usize, 7, 300, 1000, 5000] {
            let msg = encode(&data, seg);
            assert_eq!(decode(&msg).ok().as_deref(), Some(&data[..]), "segment size {seg}");
        }
        assert_eq!(decode(&encode(&[], 10)).ok().as_deref(), Some(&[][..]));
        // 39 bytes for an empty message, the size Azure shows for 1536 bytes + 39.
        assert_eq!(encode(&[], 10).len(), 39);
        assert_eq!(encode(&data[..100], SEGMENT).len(), 100 + 39);
    }

    #[test]
    fn a_flipped_byte_or_a_bad_layout_is_refused() {
        let data = vec![7u8; 100];
        let msg = encode(&data, 40);
        let mut bad = msg.clone();
        bad[HEADER_SIZE + SEGMENT_HEADER + 3] ^= 1;
        assert!(matches!(decode(&bad), Err(Fault::Crc(_))));
        let mut bad = msg.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(matches!(decode(&bad), Err(Fault::Crc(_))));
        assert!(matches!(decode(&msg[..msg.len() - 1]), Err(Fault::Malformed(_))));
        let mut longer = msg.clone();
        longer.push(0);
        assert!(matches!(decode(&longer), Err(Fault::Malformed(_))));
        let mut v2 = msg.clone();
        v2[0] = 2;
        assert!(matches!(decode(&v2), Err(Fault::Malformed(_))));
        let mut no_flag = msg;
        no_flag[9] = 0;
        assert!(matches!(decode(&no_flag), Err(Fault::Malformed(_))));
    }

    #[test]
    fn header_value_forms() {
        assert!(is_known("XSM/1.0; properties=crc64"));
        assert!(is_known("xsm/1.0;properties=CRC64"));
        assert!(!is_known("XSM/2.0; properties=crc64"));
        assert!(!is_known("XSM/1.0"));
    }
}
