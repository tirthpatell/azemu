//! Error values and the JSON body Cosmos DB sends with them.

use azemu_cosmos_sql::SqlError;
use serde_json::{json, Value};

/// A failed request: HTTP status, Cosmos error code, message, optional sub-status.
#[derive(Debug, Clone)]
pub struct CosmosError {
    pub status: u16,
    pub code: &'static str,
    /// The text inside `{"Errors":["..."]}`, or a ready JSON string when `raw_message` is set.
    pub message: String,
    pub substatus: Option<u32>,
    /// A complete JSON document used instead of `{"Errors":[message]}` (query syntax errors).
    pub raw_message: Option<String>,
    /// Extra top level field `additionalErrorInfo` (query plan for sub-status 1004).
    pub additional_info: Option<String>,
    /// Extra response headers.
    pub headers: Vec<(String, String)>,
}

pub type R<T> = Result<T, CosmosError>;

impl CosmosError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        CosmosError {
            status,
            code,
            message: message.into(),
            substatus: None,
            raw_message: None,
            additional_info: None,
            headers: Vec::new(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "BadRequest", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "Unauthorized", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "NotFound", message)
    }

    /// The generic 404 text.
    pub fn not_found_default() -> Self {
        Self::not_found("Resource Not Found. Learn more: https://aka.ms/cosmosdb-tsg-not-found")
    }

    pub fn conflict() -> Self {
        Self::new(409, "Conflict", "Entity with the specified id already exists in the system.")
    }

    pub fn precondition_failed() -> Self {
        Self::new(
            412,
            "PreconditionFailed",
            "One of the specified pre-condition is not met. Learn more: https://aka.ms/CosmosDB/sql/errors/precondition-failed",
        )
    }

    pub fn too_large() -> Self {
        Self::new(413, "RequestEntityTooLarge", "Request size is too large")
    }

    pub fn not_implemented(operation: &str) -> Self {
        Self::new(501, "NotImplemented", format!("azemu cosmos: {operation} is not implemented"))
    }

    pub fn sub(mut self, n: u32) -> Self {
        self.substatus = Some(n);
        self
    }

    /// 400 with the query engine's error document.
    pub fn from_sql(e: &SqlError) -> Self {
        let mut err = Self::bad_request(e.message.clone());
        err.raw_message = Some(e.to_cosmos_json().to_string());
        err
    }

    /// The response body for this error.
    pub fn body(&self, activity: &str) -> Value {
        let inner = match &self.raw_message {
            Some(s) => s.clone(),
            None => json!({ "Errors": [self.message] }).to_string(),
        };
        let mut v = json!({
            "code": self.code,
            "message": format!(
                "Message: {inner}\r\nActivityId: {activity}, Microsoft.Azure.Documents.Common/2.14.0"
            ),
        });
        if let Some(info) = &self.additional_info {
            v["additionalErrorInfo"] = Value::String(info.clone());
        }
        v
    }
}

/// The error for the 400 sub-status 1001 / header-versus-body partition key mismatch.
pub fn pk_mismatch() -> CosmosError {
    CosmosError::bad_request(
        "The partition key supplied in x-ms-partitionkey header has fewer components than defined in the the collection.",
    )
    .sub(1001)
}

/// Validates the name of a database, container, user, permission, stored procedure, trigger or function the way the
/// service does: at most 255 characters, no `/`, `\`, `?` or `#`.
pub fn check_id(id: &str) -> R<()> {
    if id.is_empty() {
        return Err(CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing"));
    }
    if id.chars().count() > 255 {
        return Err(CosmosError::bad_request("Id exceeds the maximum length of 255 characters."));
    }
    if id.contains(['/', '\\', '?', '#']) {
        return Err(CosmosError::bad_request("Id contains illegal chars."));
    }
    if id.ends_with(' ') {
        return Err(CosmosError::bad_request("Id ends with a space."));
    }
    Ok(())
}

/// Longest item id: 1,023 bytes (Microsoft Learn, "Service quotas and default limits", per-item limits).
pub const MAX_ITEM_ID_BYTES: usize = 1023;

/// Validates the id of an item (a document). Not the rules of [`check_id`]: on Azure the service accepts every character
/// except `/` and `\` in an item id (`USER#123` is fine, so is `a?b`), and up to 1,023 bytes. The 255 characters and the
/// refusal of `?` and `#` belong to the names of databases and containers.
pub fn check_item_id(id: &str) -> R<()> {
    if id.is_empty() {
        return Err(CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing"));
    }
    if id.len() > MAX_ITEM_ID_BYTES {
        return Err(CosmosError::bad_request(format!("Id exceeds the maximum length of {MAX_ITEM_ID_BYTES} bytes.")));
    }
    if id.contains(['/', '\\']) {
        return Err(CosmosError::bad_request("Id contains illegal chars."));
    }
    if id.ends_with(' ') {
        return Err(CosmosError::bad_request("Id ends with a space."));
    }
    Ok(())
}
