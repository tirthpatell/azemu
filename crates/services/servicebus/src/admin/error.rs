//! The error bodies of the management API: `<Error><Code>409</Code><Detail>...</Detail></Error>`,
//! with the texts the real service sends (copied from the recordings).

use azemu_core::resp::Resp;
use azemu_core::Response;

use super::util::{esc, plain};
use super::Cx;

const MORE: &str = "To know more visit https://aka.ms/sbResourceMgrExceptions.";

#[derive(Debug, Clone)]
pub enum ErrKind {
    /// 400 `SubCode=40000. Bad Request.`
    BadRequest,
    /// 400 with a specific text. `subcode` adds the `SubCode=40000. ` prefix (queues and topics have it, subscriptions do not).
    BadRequestMsg { msg: String, subcode: bool },
    /// 400 with the whole text before `TrackingId` given by the caller (the texts with a
    /// `Parameter name:` / `Actual value was` tail have their own layout).
    BadRequestRaw(String),
    /// 400 `The specified HTTP verb (PUT) is not valid.`
    Verb(String),
    /// 401 with the given detail.
    Unauthorized(String),
    /// 404 `SubCode=40400. Not Found. The Operation doesn't exist.` (update of a missing queue or topic)
    NotFoundOperation,
    /// 404 `Entity '...' was not found.` (subscriptions and rules)
    NotFoundEntity(String),
    /// 404 `No service is hosted at the specified address.` (delete of a missing entity)
    NoService,
    /// 409 `SubCode=40900. Conflict.` (create of an existing queue or topic)
    Conflict,
    /// 409 `The messaging entity '...' already exists.` (subscriptions and rules)
    AlreadyExists(String),
    /// 403 a quota of the namespace is used up.
    Quota(String),
    /// 501
    NotImplemented(String),
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub kind: ErrKind,
    /// ETag header: the real service sends the existing entity's ETag with 400/404/409.
    pub etag: Option<String>,
}

impl ApiError {
    pub fn new(kind: ErrKind) -> ApiError {
        ApiError { kind, etag: None }
    }

    pub fn with_etag(mut self, etag: String) -> ApiError {
        self.etag = Some(etag);
        self
    }

    pub fn bad_request() -> ApiError {
        ApiError::new(ErrKind::BadRequest)
    }

    pub fn bad(msg: impl Into<String>, subcode: bool) -> ApiError {
        ApiError::new(ErrKind::BadRequestMsg { msg: msg.into(), subcode })
    }

    /// A bad value of a named parameter, laid out as the service does:
    /// `<first line> <rest>\r\nParameter name: X\r\nActual value was V.`
    pub fn bad_param(first: &str, param: &str, actual: &str, subcode: bool, more_inline: bool) -> ApiError {
        let prefix = if subcode { "SubCode=40000. " } else { "" };
        let more = if more_inline { format!(" {MORE} ") } else { String::new() };
        ApiError::new(ErrKind::BadRequestRaw(format!("{prefix}{first}{more}\r\nParameter name: {param}\r\nActual value was {actual}.")))
    }

    pub fn not_found_entity(id: String) -> ApiError {
        ApiError::new(ErrKind::NotFoundEntity(id))
    }

    pub fn not_implemented(what: impl Into<String>) -> ApiError {
        ApiError::new(ErrKind::NotImplemented(what.into()))
    }

    fn status(&self) -> u16 {
        match &self.kind {
            ErrKind::BadRequest | ErrKind::BadRequestMsg { .. } | ErrKind::BadRequestRaw(_) | ErrKind::Verb(_) => 400,
            ErrKind::Unauthorized(_) => 401,
            ErrKind::Quota(_) => 403,
            ErrKind::NotFoundOperation | ErrKind::NotFoundEntity(_) | ErrKind::NoService => 404,
            ErrKind::Conflict | ErrKind::AlreadyExists(_) => 409,
            ErrKind::NotImplemented(_) => 501,
        }
    }

    /// `entity` is the path of the entity the request was about (for the tracker text).
    pub fn response(self, cx: &Cx, entity: &str) -> Response {
        let tracking = format!("TrackingId:{}_G{}", uuid::Uuid::new_v4(), cx.now.rem_euclid(16));
        let ts = format!("Timestamp:{}", plain(cx.now));
        let tracker = format!("SystemTracker:{}.servicebus.windows.net:{entity}", cx.account);
        let no_tracker = "SystemTracker:NoSystemTracker";
        let status = self.status();
        let detail = match &self.kind {
            ErrKind::BadRequest => format!("SubCode=40000. Bad Request. {MORE} . {tracking}, {tracker}, {ts}"),
            ErrKind::BadRequestMsg { msg, subcode } => {
                let prefix = if *subcode { "SubCode=40000. " } else { "" };
                format!("{prefix}{msg} {MORE} . {tracking}, {tracker}, {ts}")
            }
            ErrKind::BadRequestRaw(text) => format!("{text} {tracking}, {tracker}, {ts}"),
            ErrKind::Verb(m) => format!("The specified HTTP verb ({m}) is not valid. {MORE} . {tracking}, {tracker}, {ts}"),
            ErrKind::Unauthorized(d) => format!("{d} {tracking}, {no_tracker}, {ts}"),
            ErrKind::NotFoundOperation => format!("SubCode=40400. Not Found. The Operation doesn't exist. {MORE} . {tracking}, {tracker}, {ts}"),
            ErrKind::NotFoundEntity(id) => format!("Entity '{id}' was not found. {MORE}  {tracking}, {no_tracker}, {ts}"),
            ErrKind::NoService => format!("No service is hosted at the specified address. {tracking}, {tracker}, {ts}"),
            ErrKind::Conflict => format!(
                "SubCode=40900. Conflict. You're requesting an operation that isn't allowed in the resource's current state. {MORE} . {tracking}, {tracker}, {ts}"
            ),
            ErrKind::AlreadyExists(id) => format!("The messaging entity '{id}' already exists. {MORE}  {tracking}, {no_tracker}, {ts}"),
            ErrKind::Quota(msg) => format!("{msg} {MORE} . {tracking}, {tracker}, {ts}"),
            ErrKind::NotImplemented(what) => format!("servicebus: {what} is not implemented by azemu."),
        };
        let body = format!("<Error><Code>{status}</Code><Detail>{}</Detail></Error>", esc(&detail));
        let mut r = Resp::new(status).header("Content-Type", "application/xml; charset=utf-8").header("Server", "Microsoft-HTTPAPI/2.0");
        if let Some(e) = &self.etag {
            r = r.header("ETag", e);
        }
        r.body(body).build()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
