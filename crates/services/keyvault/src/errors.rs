//! Error responses. Everything goes through `azemu_core::json_error`, so the body
//! is always `{"error":{"code":…,"message":…[,"innererror":{"code":…}]}}`.

use azemu_core::{json_error, Response};

/// What kind of object an error is about; picks the words used in messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Secret,
    Key,
    Certificate,
}

impl Kind {
    /// "secret", "key", "certificate"
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Secret => "secret",
            Kind::Key => "key",
            Kind::Certificate => "certificate",
        }
    }

    /// Error code used when the object does not exist.
    pub fn not_found_code(self) -> &'static str {
        match self {
            Kind::Secret => "SecretNotFound",
            Kind::Key => "KeyNotFound",
            Kind::Certificate => "CertificateNotFound",
        }
    }

    /// "Secret", "Key", "Certificate"
    pub fn title(self) -> &'static str {
        match self {
            Kind::Secret => "Secret",
            Kind::Key => "Key",
            Kind::Certificate => "Certificate",
        }
    }
}

pub fn bad_parameter(message: &str) -> Response {
    json_error::response(400, "BadParameter", message)
}

pub fn forbidden(message: &str, inner: &str) -> Response {
    json_error::response_with_inner(403, "Forbidden", message, inner)
}

pub fn conflict(message: &str) -> Response {
    json_error::response(409, "Conflict", message)
}

/// 404 for an active object (or one of its versions) that does not exist.
/// `what` is `name` or `name/version`.
pub fn not_found(kind: Kind, what: &str) -> Response {
    let n = kind.noun();
    json_error::response(
        404,
        kind.not_found_code(),
        &format!(
            "A {n} with (name/id) {what} was not found in this key vault. If you recently deleted this {n} you may be able to recover it using the correct recovery command. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125182"
        ),
    )
}

/// 404 for a name that is not in the deleted state.
pub fn deleted_not_found(kind: Kind, name: &str) -> Response {
    json_error::response(404, kind.not_found_code(), &format!("Deleted {} not found: {name}", kind.title()))
}

/// 409 when the name is in the deleted (recoverable) state.
pub fn name_is_deleted(kind: Kind, name: &str) -> Response {
    let n = kind.noun();
    json_error::response_with_inner(
        409,
        "Conflict",
        &format!(
            "{} {name} is currently in a deleted but recoverable state, and its name cannot be reused; in this state, the {n} can only be recovered or purged.",
            kind.title()
        ),
        "ObjectIsDeletedButRecoverable",
    )
}

/// 409 for restoring a backup whose name is already in use. `id` is the id of the
/// object being restored (with version), as in the real message.
pub fn restore_conflict(kind: Kind, id: &str) -> Response {
    let n = kind.noun();
    conflict(&format!(
        "There was a conflict restoring the {n} '{id}'. This can happen if either: a second {n} with the same name was created after the first {n} was deleted; thus trying to restore a {n} whose name is already in use. To fix this, rename the second {n} to something else so that the restore works. The second probable cause of this exception is when multiple operations are performed in parallel against the {n}. To avoid this error, perform operations against a {n} in a sequential manner."
    ))
}

/// 501 naming the operation. `operation` is what `GET /_azemu/unsupported` shows (`KeyVault_Sign (symmetric key)`).
pub fn not_implemented(what: &str, operation: &str) -> Response {
    azemu_core::unsupported::mark(json_error::response(501, "NotImplemented", &format!("keyvault: {what}")), operation)
}

pub fn not_found_route(method: &str, path: &str) -> Response {
    json_error::response(404, "NotFound", &format!("Resource not found: {method} {path}"))
}
