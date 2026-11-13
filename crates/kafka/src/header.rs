//! Request and response framing. A frame is a 4-byte big-endian length and that many bytes. The request header is
//! version 1 (api key, version, correlation id, client id) or, for flexible versions, version 2 (plus tagged
//! fields). The response header is a correlation id, plus tagged fields for flexible versions; `ApiVersions` is the
//! exception: its response header is always the short one.

use crate::api;
use crate::error::{Error, Result};
use crate::schema::{self, request_schema, response_schema};
use crate::value::Struct;
use crate::wire::{Reader, Writer};

#[derive(Debug, Clone, PartialEq)]
pub struct RequestHeader {
    pub api_key: i16,
    pub version: i16,
    pub correlation_id: i32,
    pub client_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub header: RequestHeader,
    pub body: Struct,
}

/// Parse one frame (without its length prefix). Errors: not a request the server serves.
/// An `ApiVersions` request of a version above the served ones is `Err(Unsupported)` with the header left to the
/// caller through [`peek_header`], because the answer to it is a v0 response, not a closed connection.
pub fn read_request(frame: &[u8]) -> Result<Request> {
    let mut r = Reader::new(frame);
    let (header, flexible) = read_header(&mut r)?;
    let spec = api::served(header.api_key).ok_or(Error::Unsupported { api_key: header.api_key, version: header.version })?;
    if header.version < spec.min || header.version > spec.max {
        return Err(Error::Unsupported { api_key: header.api_key, version: header.version });
    }
    let schema = request_schema(header.api_key).ok_or(Error::Unsupported { api_key: header.api_key, version: header.version })?;
    debug_assert_eq!(flexible, schema.flexible(header.version));
    let body = schema::decode(schema, header.version, &mut r)?;
    Ok(Request { header, body })
}

/// Just the header of a frame (api key, version, correlation id, client id), without looking at the body.
pub fn peek_header(frame: &[u8]) -> Result<RequestHeader> {
    let mut r = Reader::new(frame);
    Ok(read_header(&mut r)?.0)
}

fn read_header(r: &mut Reader<'_>) -> Result<(RequestHeader, bool)> {
    let api_key = r.i16()?;
    let version = r.i16()?;
    let correlation_id = r.i32()?;
    let flexible = request_schema(api_key).map(|s| s.flexible(version)).unwrap_or(false);
    // the client id is a plain (not compact) nullable string in both header versions
    let client_id = r.string()?.map(str::to_string);
    if flexible {
        let n = r.uvarint()?;
        if n as usize > r.remaining() {
            return Err(Error::Invalid("tagged field count over the bytes left"));
        }
        for _ in 0..n {
            r.uvarint()?;
            let size = r.uvarint()? as usize;
            r.take(size)?;
        }
    }
    Ok((RequestHeader { api_key, version, correlation_id, client_id }, flexible))
}

/// A complete response frame, length prefix included.
pub fn write_response(header: &RequestHeader, body: &Struct) -> Vec<u8> {
    let mut w = Writer::with_capacity(64);
    w.i32(0); // length, filled in below
    w.i32(header.correlation_id);
    if let Some(schema) = response_schema(header.api_key) {
        // ApiVersions always answers with the short header
        if schema.flexible(header.version) && header.api_key != api::API_VERSIONS {
            w.uvarint(0);
        }
        schema::encode(schema, header.version, body, &mut w);
    }
    let len = (w.buf.len() - 4) as i32;
    w.buf[..4].copy_from_slice(&len.to_be_bytes());
    w.into_vec()
}

/// A complete request frame, for clients and tests.
pub fn write_request(header: &RequestHeader, body: &Struct) -> Vec<u8> {
    let mut w = Writer::with_capacity(64);
    w.i32(0);
    w.i16(header.api_key);
    w.i16(header.version);
    w.i32(header.correlation_id);
    w.string(header.client_id.as_deref());
    if let Some(schema) = request_schema(header.api_key) {
        if schema.flexible(header.version) {
            w.uvarint(0);
        }
        schema::encode(schema, header.version, body, &mut w);
    }
    let len = (w.buf.len() - 4) as i32;
    w.buf[..4].copy_from_slice(&len.to_be_bytes());
    w.into_vec()
}

/// Parse a response frame (without the length prefix) for a request of `header`; for clients and tests.
pub fn read_response(header: &RequestHeader, frame: &[u8]) -> Result<(i32, Struct)> {
    let mut r = Reader::new(frame);
    let corr = r.i32()?;
    let schema = response_schema(header.api_key).ok_or(Error::Unsupported { api_key: header.api_key, version: header.version })?;
    if schema.flexible(header.version) && header.api_key != api::API_VERSIONS {
        let n = r.uvarint()?;
        for _ in 0..n {
            r.uvarint()?;
            let size = r.uvarint()? as usize;
            r.take(size)?;
        }
    }
    let body = schema::decode(schema, header.version, &mut r)?;
    Ok((corr, body))
}
