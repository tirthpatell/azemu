//! Request bodies: plain or gzip.

use std::io::Read;

use azemu_core::Request;

/// Why a body could not be read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BodyError {
    /// `Content-Encoding` names something other than gzip (the value is kept for the message).
    Encoding(String),
    /// The body says gzip and is not.
    BadGzip,
    /// The decoded body is over the limit.
    TooLarge,
}

/// The body after `Content-Encoding` is undone. A body sent as it is may be `plain_limit` bytes, a gzip body may grow to
/// `decoded_limit` bytes when undone.
pub(crate) fn decode(req: &Request, plain_limit: usize, decoded_limit: usize) -> Result<Vec<u8>, BodyError> {
    let enc = req.header("content-encoding").map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default();
    match enc.as_str() {
        "" | "identity" => {
            if req.body.len() > plain_limit {
                return Err(BodyError::TooLarge);
            }
            Ok(req.body.to_vec())
        }
        "gzip" | "x-gzip" => {
            let mut out = Vec::new();
            // one byte more than the limit tells "exactly at the limit" from "over it"
            let mut dec = flate2::read::MultiGzDecoder::new(req.body.as_ref()).take(decoded_limit as u64 + 1);
            dec.read_to_end(&mut out).map_err(|_| BodyError::BadGzip)?;
            if out.len() > decoded_limit {
                return Err(BodyError::TooLarge);
            }
            Ok(out)
        }
        other => Err(BodyError::Encoding(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::request;
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn plain_and_gzip() {
        assert_eq!(decode(&request(b"abc".to_vec(), None), 10, 10).unwrap(), b"abc");
        assert_eq!(decode(&request(b"abc".to_vec(), Some("identity")), 10, 10).unwrap(), b"abc");
        assert_eq!(decode(&request(gz(b"abc"), Some("gzip")), 10, 10).unwrap(), b"abc");
        assert_eq!(decode(&request(gz(b"abc"), Some("GZIP")), 10, 10).unwrap(), b"abc");
    }

    #[test]
    fn plain_and_decoded_limits_are_separate() {
        // a gzip body may be bigger than the plain limit once undone, up to the decoded limit
        assert_eq!(decode(&request(gz(&[b'a'; 50]), Some("gzip")), 10, 100).unwrap().len(), 50);
        assert_eq!(decode(&request(gz(&[b'a'; 150]), Some("gzip")), 10, 100), Err(BodyError::TooLarge));
    }

    #[test]
    fn limits_and_errors() {
        assert_eq!(decode(&request(vec![b'a'; 11], None), 10, 100), Err(BodyError::TooLarge));
        assert_eq!(decode(&request(vec![b'a'; 10], None), 10, 100).unwrap().len(), 10);
        // a small gzip that grows past the limit when undone
        assert_eq!(decode(&request(gz(&vec![b'a'; 1000]), Some("gzip")), 10, 10), Err(BodyError::TooLarge));
        assert_eq!(decode(&request(gz(&[b'a'; 10]), Some("gzip")), 10, 10).unwrap().len(), 10);
        assert_eq!(decode(&request(b"not gzip".to_vec(), Some("gzip")), 10, 10), Err(BodyError::BadGzip));
        assert_eq!(decode(&request(b"x".to_vec(), Some("br")), 10, 10), Err(BodyError::Encoding("br".into())));
    }
}
