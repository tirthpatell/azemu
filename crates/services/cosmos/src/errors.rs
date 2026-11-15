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
    /// How `message` is written into the body (the service uses three shapes, see [`Shape`]).
    pub shape: Shape,
}

/// The three shapes of an error `message` on Azure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `Message: {"Errors":["..."]}\r\nActivityId: ...` (most 400s, and 404 / 409 of databases, containers, users and scripts).
    Wrapped,
    /// The text, then `\r\nActivityId: ...`, with no `Message:` around it (a bad page size, session token, API version, ...).
    Plain,
    /// The text, then the request times and the number of regions tried (404, 409 and 412 of items).
    Item,
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
            shape: Shape::Wrapped,
        }
    }

    /// An error whose text stands alone in the body (no `Message: {"Errors":[..]}`).
    pub fn plain(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        let mut e = Self::new(status, code, message);
        e.shape = Shape::Plain;
        e
    }

    /// An error of an item operation (404, 409, 412): the text, then the request times.
    pub fn item(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        let mut e = Self::new(status, code, message);
        e.shape = Shape::Item;
        e
    }

    /// 400 with several texts in `{"Errors":[..]}` (a bad name comes with "The request payload is invalid").
    pub fn bad_request_list(messages: &[String]) -> Self {
        let mut e = Self::bad_request(messages.first().cloned().unwrap_or_default());
        e.raw_message = Some(json!({ "Errors": messages }).to_string());
        e
    }

    /// The 500 Azure answers when it fails on input it should have refused.
    pub fn unknown() -> Self {
        Self::new(
            500,
            "InternalServerError",
            "An unknown error occurred while processing this request. If the issue persists, please contact Azure Support: http://aka.ms/azure-support",
        )
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

    /// An item that already exists (or breaks a unique key).
    pub fn conflict() -> Self {
        Self::item(409, "Conflict", "Entity with the specified id already exists in the system.")
    }

    /// A database, container, user, permission, stored procedure, trigger or function that already exists.
    pub fn resource_conflict() -> Self {
        Self::new(409, "Conflict", "Resource with specified id, name, or unique index already exists.")
    }

    /// An item that is not there.
    pub fn item_not_found() -> Self {
        Self::item(
            404,
            "NotFound",
            "Entity with the specified id does not exist in the system. More info: https://aka.ms/cosmosdb-tsg-not-found",
        )
    }

    pub fn precondition_failed() -> Self {
        Self::item(412, "PreconditionFailed", "Operation cannot be performed because one of the specified precondition is not met.")
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
        // a query that builds a value Azure cannot hold fails there with a 503, not a 400 (checked in November 2026)
        if e.kind == azemu_cosmos_sql::ErrorKind::LimitExceeded && e.message.contains("would build a value larger than") {
            return Self::new(
                503,
                "ServiceUnavailable",
                "Service is currently unavailable. More info: https://aka.ms/cosmosdb-tsg-service-unavailable",
            );
        }
        let mut err = Self::bad_request(e.message.clone());
        match e.shape {
            // the gateway's own text: `{"Errors":["..."]}`, which is what a plain message becomes
            azemu_cosmos_sql::Shape::Gateway => {}
            // the text as it is, with no `Message:` around it
            azemu_cosmos_sql::Shape::Text => err.shape = Shape::Plain,
            _ => err.raw_message = Some(e.to_cosmos_json().to_string()),
        }
        err.substatus = e.substatus;
        err
    }

    /// The response body for this error.
    pub fn body(&self, activity: &str) -> Value {
        let message = match self.shape {
            Shape::Item => {
                // as on Azure: the text, the times of the request, the number of regions tried
                let now = azemu_core::time::OffsetDateTime::now_utc();
                let t = azemu_core::time::iso8601_7(now);
                format!("{}, \r\nRequestStartTime: {t}, RequestEndTime: {t},  Number of regions attempted:1\r\n", self.message)
            }
            Shape::Plain => {
                format!("{}\r\nActivityId: {activity}, Microsoft.Azure.Documents.Common/2.14.0", self.message)
            }
            Shape::Wrapped => {
                let inner = match &self.raw_message {
                    Some(s) => s.clone(),
                    None => json!({ "Errors": [self.message] }).to_string(),
                };
                format!("Message: {inner}\r\nActivityId: {activity}, Microsoft.Azure.Documents.Common/2.14.0")
            }
        };
        let mut v = json!({
            "code": self.code,
            "message": message,
        });
        if let Some(info) = &self.additional_info {
            v["additionalErrorInfo"] = Value::String(info.clone());
        }
        v
    }
}

/// The 400 for a partition key header that has fewer components than the container's key (also no header at all), and for a
/// header that does not match the document's own key. Both carry sub-status 1001 on Azure.
pub fn pk_mismatch() -> CosmosError {
    CosmosError::bad_request(
        "PartitionKey extracted from document doesn't match the one specified in the header. Learn more: https://aka.ms/CosmosDB/sql/errors/wrong-pk-value",
    )
    .sub(1001)
}

pub fn pk_fewer_components() -> CosmosError {
    CosmosError::bad_request(
        "The partition key supplied in x-ms-partitionkey header has fewer components than defined in the the collection.",
    )
    .sub(1001)
}

/// A header with more components than the container's key has.
pub fn pk_more_components() -> CosmosError {
    CosmosError::plain(
        400,
        "BadRequest",
        "Partition key provided either doesn't correspond to definition in the collection or doesn't match partition key field values specified in the document.",
    )
    .sub(1001)
}

const PAYLOAD_INVALID: &str = "The request payload is invalid. Ensure to provide a valid request payload.";
const MISSING_ID: &str = "Learn more: https://aka.ms/CosmosDB/sql/errors/missing-id";

/// What a name belongs to: the rules differ (Azure refuses `/` and `\` in the names of databases and containers only).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NameOf {
    Database,
    Container,
    /// A user, permission, stored procedure, trigger or user-defined function.
    Other,
}

/// The "required properties are missing" error. Everything but an item adds "The request payload is invalid".
pub fn missing_properties(names: &[&str], item: bool) -> CosmosError {
    let list: String = names.iter().map(|n| format!("{n}; ")).collect();
    let first = format!("The input content is invalid because the required properties - '{list}' - are missing. {MISSING_ID}");
    if item {
        CosmosError::bad_request(first)
    } else {
        CosmosError::bad_request_list(&[first, PAYLOAD_INVALID.to_string()])
    }
}

fn name_invalid(shown: Option<&str>, limit: usize) -> String {
    match shown {
        Some(n) => format!("The input name '{n}' is invalid. Ensure to provide a unique non-empty string less than '{limit}' characters."),
        None => format!("The input name is invalid. Ensure to provide a unique non-empty string less than '{limit}' characters."),
    }
}

/// The `id` of a database, container, user, permission, script: a string of at most 254 characters. Azure refuses an empty
/// one, `/` and `\` in the name of a database or container, and nothing else (a name may hold `#`, `?`, spaces).
pub fn name_of(kind: NameOf, body: &serde_json::Map<String, Value>) -> R<String> {
    let id = match body.get("id") {
        Some(Value::String(s)) => s.clone(),
        // a container without an id is a name that is empty; a number is "one of the specified inputs"
        None if kind == NameOf::Container => String::new(),
        None => return Err(missing_properties(&["id"], false)),
        Some(_) if kind != NameOf::Other => return Err(CosmosError::bad_request("One of the specified inputs is invalid")),
        Some(other) => {
            return Err(CosmosError::bad_request_list(&[name_invalid(Some(&json_head(other)), 255), PAYLOAD_INVALID.to_string()]))
        }
    };
    if id.is_empty() {
        return Err(CosmosError::bad_request_list(&[name_invalid(Some(""), 255), PAYLOAD_INVALID.to_string()]));
    }
    if id.chars().count() >= 255 {
        return Err(CosmosError::bad_request_list(&[name_invalid(None, 255), PAYLOAD_INVALID.to_string()]));
    }
    if kind != NameOf::Other && id.contains(['/', '\\']) {
        return Err(CosmosError::bad_request("The input name presented contains invalid character."));
    }
    Ok(id)
}

/// The text Azure shows for an `id` that is not a string: the start of its JSON.
fn json_head(v: &Value) -> String {
    match v {
        Value::Array(_) => "[".to_string(),
        Value::Object(_) => "{".to_string(),
        other => other.to_string(),
    }
}

/// Longest item id: 1,023 bytes (Microsoft Learn, "Service quotas and default limits", per-item limits).
pub const MAX_ITEM_ID_BYTES: usize = 1023;

/// The `id` of an item. Checked against Azure in November 2026: every character is allowed (`/`, `\`, `#`, a space at the
/// start or the end), the id is a string, not empty, under 1,024 characters. An id of 1,024 characters or more is a
/// 400; an id that is shorter but holds more than 1,023 bytes in UTF-8 (so it has multi-byte characters) is a 500.
pub fn item_id(m: &serde_json::Map<String, Value>) -> R<String> {
    let id = match m.get("id") {
        Some(Value::String(s)) => s,
        None => return Err(missing_properties(&["id"], true)),
        Some(other) => return Err(CosmosError::bad_request(name_invalid(Some(&json_head(other)), 1024))),
    };
    if id.is_empty() {
        return Err(CosmosError::bad_request(name_invalid(Some(""), 1024)));
    }
    if id.chars().count() > MAX_ITEM_ID_BYTES {
        return Err(CosmosError::bad_request(name_invalid(None, 1024)));
    }
    if id.len() > MAX_ITEM_ID_BYTES {
        return Err(CosmosError::unknown());
    }
    Ok(id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn item_ids_follow_what_azure_answered() {
        for ok in ["a", " ", "x ", "a/b", "a\\b", "USER#1", &"a".repeat(1023), &"€".repeat(341)] {
            assert_eq!(item_id(&obj(json!({ "id": ok }))).unwrap(), ok);
        }
        // 1,024 characters: 400 without the name; more than 1,023 bytes in fewer characters: 500
        let e = item_id(&obj(json!({ "id": "a".repeat(1024) }))).unwrap_err();
        assert_eq!(e.status, 400);
        assert_eq!(e.message, "The input name is invalid. Ensure to provide a unique non-empty string less than '1024' characters.");
        assert_eq!(item_id(&obj(json!({ "id": "€".repeat(342) }))).unwrap_err().status, 500);
        assert_eq!(item_id(&obj(json!({ "id": "ä".repeat(1024) }))).unwrap_err().status, 400);
        // empty, not a string, absent
        assert!(item_id(&obj(json!({ "id": "" }))).unwrap_err().message.starts_with("The input name '' is invalid."));
        assert!(item_id(&obj(json!({ "id": 5 }))).unwrap_err().message.starts_with("The input name '5' is invalid."));
        assert!(item_id(&obj(json!({ "id": ["x"] }))).unwrap_err().message.starts_with("The input name '[' is invalid."));
        assert!(item_id(&obj(json!({}))).unwrap_err().message.contains("'id; ' - are missing. Learn more:"));
    }

    #[test]
    fn names_of_databases_and_containers_refuse_a_slash_and_nothing_else() {
        for ok in ["a#b", "a?b", "a b", "x ", " ", &"d".repeat(254)] {
            assert_eq!(name_of(NameOf::Database, &obj(json!({ "id": ok }))).unwrap(), ok);
        }
        for bad in ["a/b", "a\\b"] {
            assert_eq!(
                name_of(NameOf::Container, &obj(json!({ "id": bad }))).unwrap_err().message,
                "The input name presented contains invalid character."
            );
            assert!(name_of(NameOf::Other, &obj(json!({ "id": bad }))).is_ok(), "users and scripts take it");
        }
        for len in [255, 256] {
            let e = name_of(NameOf::Database, &obj(json!({ "id": "d".repeat(len) }))).unwrap_err();
            assert!(e.raw_message.unwrap().contains("The request payload is invalid. Ensure to provide a valid request payload."));
        }
        assert!(name_of(NameOf::Container, &obj(json!({}))).unwrap_err().message.starts_with("The input name '' is invalid."));
        assert!(name_of(NameOf::Database, &obj(json!({}))).unwrap_err().raw_message.unwrap().contains("missing-id"));
        assert_eq!(name_of(NameOf::Database, &obj(json!({ "id": 5 }))).unwrap_err().message, "One of the specified inputs is invalid");
    }

    #[test]
    fn the_three_shapes_of_an_error_body() {
        let wrapped = CosmosError::bad_request("x").body("act");
        assert_eq!(wrapped["message"], "Message: {\"Errors\":[\"x\"]}\r\nActivityId: act, Microsoft.Azure.Documents.Common/2.14.0");
        let plain = CosmosError::plain(400, "BadRequest", "x").body("act");
        assert_eq!(plain["message"], "x\r\nActivityId: act, Microsoft.Azure.Documents.Common/2.14.0");
        let item = CosmosError::conflict().body("act");
        let m = item["message"].as_str().unwrap();
        assert!(m.starts_with("Entity with the specified id already exists in the system., \r\nRequestStartTime: "), "{m}");
        assert!(m.ends_with("Number of regions attempted:1\r\n"), "{m}");
    }
}
