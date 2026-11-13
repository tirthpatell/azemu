//! The ARM error envelope: `{"error": {"code": ..., "message": ...}}`.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ArmError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Vec<Value>,
}

pub type ArmResult<T> = Result<T, ArmError>;

impl ArmError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> ArmError {
        ArmError { status, code: code.to_string(), message: message.into(), details: Vec::new() }
    }

    pub fn bad_request(code: &str, message: impl Into<String>) -> ArmError {
        ArmError::new(400, code, message)
    }

    pub fn not_found(code: &str, message: impl Into<String>) -> ArmError {
        ArmError::new(404, code, message)
    }

    pub fn conflict(code: &str, message: impl Into<String>) -> ArmError {
        ArmError::new(409, code, message)
    }

    /// An operation that is not built answers 501 and names itself.
    pub fn not_implemented(what: impl Into<String>) -> ArmError {
        ArmError::new(501, "NotImplemented", format!("azemu arm: {}", what.into()))
    }

    pub fn missing_api_version() -> ArmError {
        ArmError::bad_request("MissingApiVersionParameter", "The api-version query parameter (?api-version=) is required for all requests.")
    }

    pub fn group_not_found(name: &str) -> ArmError {
        ArmError::not_found("ResourceGroupNotFound", format!("Resource group '{name}' could not be found."))
    }

    pub fn resource_not_found(type_path: &str, name: &str, rg: &str) -> ArmError {
        ArmError::not_found(
            "ResourceNotFound",
            format!(
                "The Resource '{type_path}/{name}' under resource group '{rg}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix"
            ),
        )
    }

    pub fn body(&self) -> Value {
        let mut e = json!({"code": self.code, "message": self.message});
        if !self.details.is_empty() {
            e["details"] = Value::Array(self.details.clone());
        }
        json!({ "error": e })
    }
}
