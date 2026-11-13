//! Things shared by secrets, keys and certificates: names, versions, attributes,
//! tags, request bodies and the JSON helpers.

use std::collections::BTreeMap;

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::errors::bad_parameter;

pub const RECOVERY_LEVEL: &str = "Recoverable+Purgeable";
pub const RECOVERABLE_DAYS: i64 = 90;

/// Longest value a secret may have (25 KiB).
pub const MAX_SECRET_BYTES: usize = 25 * 1024;

pub type Tags = BTreeMap<String, String>;

/// Names are 1-127 characters of `0-9 a-z A-Z -`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 127 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub fn invalid_name(name: &str) -> Response {
    bad_parameter(&format!("The request URI contains an invalid name: {name}"))
}

/// A fresh version id: 32 lowercase hex characters.
pub fn new_version() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn b64u(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Decode base64url; padding and the standard alphabet are tolerated.
pub fn b64u_decode(text: &str) -> Option<Vec<u8>> {
    let t: String = text
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    URL_SAFE_NO_PAD.decode(t.as_bytes()).ok()
}

pub fn json_response(status: u16, v: &Value) -> Response {
    Resp::new(status).json(v).build()
}

/// The request body as a JSON object. An empty body is an empty object.
pub fn body_object(req: &Request) -> Result<Map<String, Value>, Response> {
    if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&req.body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(bad_parameter("The request body must be a JSON object.")),
        Err(_) => Err(bad_parameter("The request body is not valid JSON.")),
    }
}

/// Attributes of one version of a secret, key or certificate.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Attrs {
    pub enabled: bool,
    pub nbf: Option<i64>,
    pub exp: Option<i64>,
    pub created: i64,
    pub updated: i64,
}

impl Attrs {
    pub fn new(now: i64) -> Attrs {
        Attrs { enabled: true, nbf: None, exp: None, created: now, updated: now }
    }

    /// The JSON `attributes` object, without the key-only fields.
    pub fn to_json(&self, p: &Profile) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("enabled".into(), json!(self.enabled));
        if let Some(n) = self.nbf {
            m.insert("nbf".into(), json!(n));
        }
        if let Some(e) = self.exp {
            m.insert("exp".into(), json!(e));
        }
        m.insert("created".into(), json!(self.created));
        m.insert("updated".into(), json!(self.updated));
        m.insert("recoveryLevel".into(), json!(RECOVERY_LEVEL));
        if p.recoverable_days {
            m.insert("recoverableDays".into(), json!(RECOVERABLE_DAYS));
        }
        m
    }

    /// Apply the fields present in a request's `attributes` object.
    pub fn apply(&mut self, patch: &AttrPatch) {
        if let Some(e) = patch.enabled {
            self.enabled = e;
        }
        if let Some(n) = patch.nbf {
            self.nbf = n;
        }
        if let Some(e) = patch.exp {
            self.exp = e;
        }
    }
}

/// The `attributes` of a request: only the fields that were sent.
/// `Some(None)` means an explicit `null` (clears the value).
#[derive(Default, Debug, Clone)]
pub struct AttrPatch {
    pub enabled: Option<bool>,
    pub nbf: Option<Option<i64>>,
    pub exp: Option<Option<i64>>,
}

fn int_field(m: &Map<String, Value>, name: &str) -> Result<Option<Option<i64>>, Response> {
    match m.get(name) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => match v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)) {
            Some(n) if v.is_number() => Ok(Some(Some(n))),
            _ => Err(bad_parameter(&format!("The attribute '{name}' must be a number of seconds since 1970-01-01T00:00:00Z."))),
        },
    }
}

pub fn parse_attr_patch(body: &Map<String, Value>) -> Result<AttrPatch, Response> {
    let Some(a) = body.get("attributes") else {
        return Ok(AttrPatch::default());
    };
    let m = match a {
        Value::Null => return Ok(AttrPatch::default()),
        Value::Object(m) => m,
        _ => return Err(bad_parameter("The 'attributes' field must be an object.")),
    };
    let enabled = match m.get("enabled") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(bad_parameter("The attribute 'enabled' must be true or false.")),
    };
    Ok(AttrPatch { enabled, nbf: int_field(m, "nbf")?, exp: int_field(m, "exp")? })
}

/// `tags` from a request body. `None` when the field is absent.
pub fn parse_tags(body: &Map<String, Value>) -> Result<Option<Tags>, Response> {
    match body.get("tags") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(Tags::new())),
        Some(Value::Object(m)) => {
            let mut out = Tags::new();
            for (k, v) in m {
                match v {
                    Value::String(s) => {
                        out.insert(k.clone(), s.clone());
                    }
                    _ => return Err(bad_parameter("Tag values must be strings.")),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(bad_parameter("The 'tags' field must be an object of strings.")),
    }
}

pub fn tags_json(tags: &Tags) -> Value {
    Value::Object(tags.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
}

/// Required string field.
pub fn str_field<'a>(body: &'a Map<String, Value>, name: &str) -> Result<&'a str, Response> {
    match body.get(name) {
        Some(Value::String(s)) => Ok(s),
        Some(Value::Null) | None => Err(bad_parameter(&format!("The parameter '{name}' is required."))),
        Some(_) => Err(bad_parameter(&format!("The parameter '{name}' must be a string."))),
    }
}

/// Optional string field.
pub fn opt_str_field<'a>(body: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>, Response> {
    match body.get(name) {
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(bad_parameter(&format!("The parameter '{name}' must be a string."))),
    }
}

/// Base64url field, decoded.
pub fn b64_field(body: &Map<String, Value>, name: &str) -> Result<Vec<u8>, Response> {
    let s = str_field(body, name)?;
    b64u_decode(s).ok_or_else(|| bad_parameter(&format!("The parameter '{name}' is not valid base64url.")))
}

pub fn now_secs(req: &Request) -> i64 {
    req.ctx.time.unix_timestamp()
}

/// Which optional attribute fields the requested `api-version` returns. The
/// recordings show these appear as the service evolved: `recoverableDays` after
/// 7.0, `exportable` from 7.4, `hsmPlatform` from 7.5.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub recoverable_days: bool,
    pub exportable: bool,
    pub hsm_platform: bool,
    /// `key_size` inside the key's `attributes` (RSA and symmetric keys).
    pub key_size: bool,
}

impl Profile {
    pub fn of(req: &Request) -> Profile {
        let v = req.query.get("api-version").unwrap_or("");
        // 7.<minor>, or a date style version (2016-10-01 is old, 2025+ is new).
        let minor: Option<u32> =
            v.strip_prefix("7.").and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|m| m.parse().ok());
        match minor {
            Some(m) => Profile { recoverable_days: m >= 1, exportable: m >= 4, hsm_platform: m >= 5, key_size: false },
            None if v.starts_with("2016") => Profile { recoverable_days: false, exportable: false, hsm_platform: false, key_size: false },
            None => Profile { recoverable_days: true, exportable: true, hsm_platform: true, key_size: true },
        }
    }
}

/// What a handler needs to build ids and bodies for this request.
pub struct Ctx {
    pub base: String,
    pub profile: Profile,
}

impl Ctx {
    pub fn of(req: &Request) -> Ctx {
        Ctx { base: req.ctx.endpoint.base_url(), profile: Profile::of(req) }
    }
}
