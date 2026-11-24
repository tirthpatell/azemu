//! The Logs Ingestion API (Data Collection Rules): `POST /dataCollectionRules/{dcr}/streams/{stream}?api-version=...`.
//!
//! Rules and streams are not declared anywhere: any name is accepted and kept as it came. What Azure does for a rule or a
//! stream that does not exist (404 or `InvalidStream`) is therefore never answered. Checked against the documentation
//! and the SDKs' source, not against Azure (there was no Data Collection Rule to try).

use azemu_core::json_error;
use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use serde_json::Value;

use crate::body::{self, BodyError};
use crate::store::LogRow;
use crate::MonitorService;

/// "Maximum size of API call: 1 MB, both compressed and uncompressed data" (Azure Monitor service limits). Applied to the
/// body as it is sent.
pub(crate) const MAX_CALL_BYTES: usize = 1 << 20;
/// What a gzip body may grow to when undone. The documented 1 MB would refuse what the Python SDK itself sends: it cuts its
/// chunks at 1 MiB of JSON counted without the commas and spaces between the records, so a chunk can be a little over
/// 1 MiB when the records are small. A quarter more leaves room for that and still refuses a body that is plainly too big.
pub(crate) const MAX_DECODED_BYTES: usize = MAX_CALL_BYTES + MAX_CALL_BYTES / 4;

pub(crate) fn too_large() -> Response {
    json_error::response(413, "ContentLengthLimitExceeded", "The request body is over the limit of 1 MB (compressed or uncompressed).")
}

impl MonitorService {
    pub(crate) fn upload_logs(&self, req: &Request, dcr: &str, stream: &str) -> Response {
        // "Authorization: Yes" in the documentation: a bearer token is needed, and nothing in it is checked
        let has_token = req.header("authorization").is_some_and(|v| {
            let v = v.trim();
            v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ")
        });
        if !has_token {
            return json_error::response(
                401,
                "Unauthorized",
                "The request needs a bearer token (Authorization: Bearer <token>) for the audience https://monitor.azure.com.",
            );
        }
        let bytes = match body::decode(req, MAX_CALL_BYTES, MAX_DECODED_BYTES) {
            Ok(b) => b,
            Err(BodyError::TooLarge) => return too_large(),
            Err(BodyError::BadGzip) => {
                return json_error::response(400, "InvalidRequest", "The body says Content-Encoding: gzip and is not gzip data.")
            }
            Err(BodyError::Encoding(e)) => {
                return json_error::response(400, "InvalidRequest", &format!("Unsupported Content-Encoding: {e}. Use gzip or none."))
            }
        };
        let rows = match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Array(rows)) => rows,
            Ok(_) => return json_error::response(400, "InvalidRequest", "The body must be a JSON array of records."),
            Err(e) => return json_error::response(400, "InvalidRequest", &format!("The body is not valid JSON: {e}")),
        };
        if rows.iter().any(|r| !r.is_object()) {
            return json_error::response(400, "InvalidRequest", "Every record in the array must be a JSON object.");
        }
        let received = azemu_core::time::iso8601_7(req.ctx.time);
        let kept: Vec<(LogRow, usize)> = rows
            .into_iter()
            .filter_map(|row| {
                // kept as text: see `LogRow::row`
                let row = serde_json::value::RawValue::from_string(row.to_string()).ok()?;
                let row = LogRow { received: received.clone(), dcr: dcr.to_string(), stream: stream.to_string(), row };
                let size = row.held_bytes();
                Some((row, size))
            })
            .collect();
        let dropped = self.logs.push_all(kept);
        if dropped > 0 {
            tracing::debug!(dropped, "monitor: the oldest log rows were dropped to stay inside the limits");
        }
        Resp::new(204u16).header("x-ms-request-id", &req.ctx.request_id).build()
    }
}
