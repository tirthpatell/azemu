//! The Entra ID token endpoint (v2 and v1) and the device code endpoint.
//!
//! Everything a grant needs later (refresh tokens, device codes, authorization
//! codes) is sealed into the opaque string itself, so the service keeps no state and
//! codes from an earlier run still work. Consequence: codes are not single use.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::jwt::{self, AccessToken, ClientAuth, User, DEFAULT_USER};
use crate::msi::lifetime;
use crate::util::{b64, entra_timestamp, new_guid, tenant_guid, unb64, Params};
use crate::IdentityService;

const RT_PREFIX: &str = "azemu.rt.";
const DC_PREFIX: &str = "azemu.dc.";
pub const AC_PREFIX: &str = "azemu.ac.";

const OIDC_SCOPES: [&str; 4] = ["openid", "profile", "email", "offline_access"];
const GRAPH: &str = "https://graph.microsoft.com";
/// The device code grant as RFC 8628 and Python MSAL write it, and the short form msal-go and msal-node send.
const DEVICE_CODE_URN: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEVICE_CODE_SHORT: &str = "device_code";
const JWT_BEARER_ASSERTION: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// The contents of a refresh token, device code or authorization code.
#[derive(Serialize, Deserialize, Default)]
pub struct Blob {
    /// client id
    pub c: String,
    /// scope (v2) or resource (v1), as requested
    pub s: String,
    /// tenant segment
    pub t: String,
    /// user name
    pub u: String,
    #[serde(default)]
    pub n: Option<String>,
    /// expiry, unix seconds; 0 = none
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub v1: bool,
}

pub fn seal(prefix: &str, b: &Blob) -> String {
    format!("{prefix}{}", b64(&serde_json::to_vec(b).unwrap_or_default()))
}

fn open(prefix: &str, s: &str) -> Option<Blob> {
    serde_json::from_slice(&unb64(s.strip_prefix(prefix)?)?).ok()
}

/// Entra's error body. `code` is the number after `AADSTS`.
pub fn entra_error(req: &Request, status: u16, error: &str, code: u32, msg: &str) -> Response {
    let trace = new_guid();
    let corr = req
        .header("client-request-id")
        .filter(|v| crate::util::is_guid(v))
        .map(str::to_string)
        .unwrap_or_else(|| req.ctx.request_id.clone());
    let ts = entra_timestamp(req.ctx.time);
    Resp::new(status)
        .header("Cache-Control", "no-store, no-cache")
        .header("Pragma", "no-cache")
        .header("client-request-id", &corr)
        .header("x-ms-request-id", &trace)
        .json(&json!({
            "error": error,
            "error_description": format!("AADSTS{code}: {msg}\r\nTrace ID: {trace}\r\nCorrelation ID: {corr}\r\nTimestamp: {ts}"),
            "error_codes": [code],
            "timestamp": ts,
            "trace_id": trace,
            "correlation_id": corr,
            "error_uri": format!("https://login.microsoftonline.com/error?code={code}"),
        }))
        .build()
}

/// A requested scope, split into the resource it is for and the permissions.
pub struct Scope {
    pub aud: String,
    pub perms: Vec<String>,
    pub oidc: Vec<String>,
    pub raw: String,
}

impl Scope {
    pub fn has_oidc(&self, name: &str) -> bool {
        self.oidc.iter().any(|s| s.eq_ignore_ascii_case(name))
    }

    fn scp(&self) -> String {
        let v: Vec<&str> = self.perms.iter().map(|p| if p == ".default" { "user_impersonation" } else { p.as_str() }).collect();
        if v.is_empty() {
            "user_impersonation".to_string()
        } else {
            v.join(" ")
        }
    }

    /// The `scope` value of a v2 response: what was granted (`.default` stands for
    /// `user_impersonation`), the `.default` entry as asked for, every other scope as it was written in
    /// the request, and the OpenID scopes. MSAL (Go: `findDeclinedScopes`) refuses a response that
    /// leaves out a scope it asked for, and the Go SDK asks for `{resource}/.default`; Entra lists the
    /// `.default` scope in its answer too.
    fn granted(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut add = |s: String| {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&s)) {
                out.push(s);
            }
        };
        for p in &self.perms {
            if p == ".default" {
                add(format!("{}/user_impersonation", self.aud));
                add(format!("{}/.default", self.aud));
            } else {
                add(format!("{}/{p}", self.aud));
            }
        }
        for t in self.raw.split_whitespace() {
            if !OIDC_SCOPES.contains(&t.to_ascii_lowercase().as_str()) {
                add(t.to_string());
            }
        }
        for o in &self.oidc {
            add(o.clone());
        }
        out.join(" ")
    }
}

fn split_resource(t: &str) -> (String, String) {
    if let Some(a) = t.strip_suffix("/.default") {
        return (a.to_string(), ".default".to_string());
    }
    if let Some(i) = t.find("://") {
        let rest = &t[i + 3..];
        return match rest.rfind('/') {
            Some(j) => (t[..i + 3 + j].to_string(), rest[j + 1..].to_string()),
            None => (t.to_string(), ".default".to_string()),
        };
    }
    match t.rsplit_once('/') {
        Some((a, p)) => (a.to_string(), p.to_string()),
        None => (GRAPH.to_string(), t.to_string()),
    }
}

/// `Err((error, aadsts code, message))`.
pub fn parse_scope(raw: &str, v1: bool, v1_scope_param: &str) -> Result<Scope, (&'static str, u32, String)> {
    if v1 {
        let oidc = v1_scope_param
            .split_whitespace()
            .filter(|s| OIDC_SCOPES.contains(&s.to_ascii_lowercase().as_str()))
            .map(str::to_string)
            .collect();
        let aud = if raw.is_empty() { GRAPH.to_string() } else { raw.to_string() };
        return Ok(Scope { aud, perms: vec![], oidc, raw: raw.to_string() });
    }
    let mut aud: Option<String> = None;
    let mut perms = vec![];
    let mut oidc = vec![];
    for t in raw.split_whitespace() {
        if OIDC_SCOPES.contains(&t.to_ascii_lowercase().as_str()) {
            oidc.push(t.to_string());
            continue;
        }
        let (a, p) = split_resource(t);
        match &aud {
            Some(prev) if !prev.eq_ignore_ascii_case(&a) => {
                return Err((
                    "invalid_scope",
                    28000,
                    "Provided value for the input parameter scope is not valid because it contains more than one resource.".into(),
                ));
            }
            Some(_) => {}
            None => aud = Some(a),
        }
        perms.push(p);
    }
    Ok(Scope { aud: aud.unwrap_or_else(|| GRAPH.to_string()), perms, oidc, raw: raw.to_string() })
}

struct Tc<'a> {
    svc: &'a IdentityService,
    req: &'a Request,
    base: &'a str,
    tenant: &'a str,
    tid: String,
    v1: bool,
    now: i64,
    life: i64,
    p: Params,
    client_id: String,
    basic_secret: Option<String>,
}

fn basic_auth(req: &Request) -> Option<(String, String)> {
    let v = req.header("authorization")?;
    let enc = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(enc.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (id, secret) = s.split_once(':')?;
    let dec = |x: &str| percent_encoding::percent_decode_str(x).decode_utf8_lossy().into_owned();
    Some((dec(id), dec(secret)))
}

/// `POST /{tenant}/oauth2/v2.0/token` and `POST /{tenant}/oauth2/token` (`v1`).
pub async fn token(svc: &IdentityService, req: &Request, base: &str, tenant: &str, v1: bool) -> Response {
    let p = Params::parse(&String::from_utf8_lossy(&req.body));
    let q = Params::parse(req.query.raw());
    let basic = basic_auth(req);
    let Some(grant) = p.nonempty("grant_type").map(str::to_string) else {
        return entra_error(req, 400, "invalid_request", 900144, "The request body must contain the following parameter: 'grant_type'.");
    };
    let known = [
        "client_credentials",
        "refresh_token",
        "password",
        "authorization_code",
        DEVICE_CODE_URN,
        DEVICE_CODE_SHORT,
        "urn:ietf:params:oauth:grant-type:jwt-bearer",
    ];
    if !known.contains(&grant.as_str()) {
        return entra_error(req, 400, "unsupported_grant_type", 70003, &format!("The app requested an unsupported grant type '{grant}'."));
    }
    let Some(client_id) =
        p.nonempty("client_id").map(str::to_string).or_else(|| basic.as_ref().map(|b| b.0.clone()).filter(|s| !s.is_empty()))
    else {
        return entra_error(req, 400, "invalid_request", 900144, "The request body must contain the following parameter: 'client_id'.");
    };
    let tc = Tc {
        svc,
        req,
        base,
        tenant,
        tid: tenant_guid(tenant),
        v1,
        now: req.ctx.time.unix_timestamp(),
        life: lifetime(&q),
        p,
        client_id,
        basic_secret: basic.map(|b| b.1),
    };
    match grant.as_str() {
        "client_credentials" => tc.client_credentials().await,
        "refresh_token" => tc.refresh().await,
        "password" => tc.password().await,
        "authorization_code" => tc.auth_code().await,
        DEVICE_CODE_URN | DEVICE_CODE_SHORT => tc.device_code().await,
        _ => tc.on_behalf_of().await,
    }
}

impl Tc<'_> {
    fn err(&self, status: u16, error: &str, code: u32, msg: &str) -> Response {
        entra_error(self.req, status, error, code, msg)
    }

    fn missing(&self, name: &str) -> Response {
        self.err(400, "invalid_request", 900144, &format!("The request body must contain the following parameter: '{name}'."))
    }

    fn scope_from(&self, stored: Option<&Blob>) -> Result<Scope, Response> {
        let (v1, raw) = if self.v1 {
            (
                true,
                self.p
                    .nonempty("resource")
                    .map(str::to_string)
                    .or_else(|| stored.filter(|b| b.v1).map(|b| b.s.clone()))
                    .unwrap_or_default(),
            )
        } else {
            (
                false,
                self.p.nonempty("scope").map(str::to_string).or_else(|| stored.filter(|b| !b.v1).map(|b| b.s.clone())).unwrap_or_default(),
            )
        };
        let scope_param = self.p.get("scope").unwrap_or("");
        parse_scope(&raw, v1, scope_param).map_err(|(e, c, m)| self.err(400, e, c, &m))
    }

    /// Client credentials need an explicit resource.
    fn app_scope(&self) -> Result<Scope, Response> {
        if self.v1 {
            if self.p.nonempty("resource").is_none() {
                return Err(self.missing("resource"));
            }
        } else {
            let Some(raw) = self.p.nonempty("scope") else {
                return Err(self.missing("scope"));
            };
            let s = self.scope_from(None)?;
            if s.perms.is_empty() || s.perms.iter().any(|p| p != ".default") {
                return Err(self.err(
                    400,
                    "invalid_scope",
                    70011,
                    &format!("The provided value for the input parameter 'scope' is not valid. The scope {raw} is not valid. Client credential flows must have a scope value with /.default suffixed to the resource identifier (application ID URI)."),
                ));
            }
            return Ok(s);
        }
        self.scope_from(None)
    }

    fn client_auth(&self) -> Result<ClientAuth, Response> {
        let secret = self.p.nonempty("client_secret").or(self.basic_secret.as_deref().filter(|s| !s.is_empty()));
        if secret.is_some() {
            return Ok(ClientAuth::Secret);
        }
        if self.p.nonempty("client_assertion").is_some() {
            let ty = self.p.nonempty("client_assertion_type");
            if ty != Some(JWT_BEARER_ASSERTION) {
                return Err(self.err(400, "invalid_request", 900144, "The request body must contain the following parameter: 'client_assertion_type' with the value 'urn:ietf:params:oauth:client-assertion-type:jwt-bearer'."));
            }
            return Ok(ClientAuth::Assertion);
        }
        Ok(ClientAuth::Public)
    }

    fn confidential(&self, grant: &str) -> Result<ClientAuth, Response> {
        match self.client_auth()? {
            ClientAuth::Public => Err(self.err(
                401,
                "invalid_client",
                7000216,
                &format!("'client_assertion', 'client_secret' or 'request' is required for the '{grant}' grant type."),
            )),
            a => Ok(a),
        }
    }

    // -- grants -------------------------------------------------------------

    async fn client_credentials(&self) -> Response {
        let auth = match self.confidential("client_credentials") {
            Ok(a) => a,
            Err(r) => return r,
        };
        let scope = match self.app_scope() {
            Ok(s) => s,
            Err(r) => return r,
        };
        let key = match self.svc.signing_key().await {
            Ok(k) => k,
            Err(e) => return self.err(500, "server_error", 90033, &e),
        };
        let claims = jwt::access_claims(&AccessToken {
            tenant_id: &self.tid,
            aud: &scope.aud,
            client_id: &self.client_id,
            auth,
            user: None,
            scp: None,
            oid: None,
            iat: self.now,
            lifetime: self.life,
        });
        let access = match jwt::sign(&key, &claims) {
            Ok(t) => t,
            Err(e) => return self.err(500, "server_error", 90033, &e),
        };
        let mut m = Map::new();
        self.put_common(&mut m, &scope, access);
        self.respond(m)
    }

    /// Fields every successful response has; v1 sends numbers as strings and adds dates.
    fn put_common(&self, m: &mut Map<String, Value>, scope: &Scope, access: String) {
        m.insert("token_type".into(), json!("Bearer"));
        if self.v1 {
            m.insert("expires_in".into(), json!(self.life.to_string()));
            m.insert("ext_expires_in".into(), json!(self.life.to_string()));
            m.insert("expires_on".into(), json!((self.now + self.life).to_string()));
            m.insert("not_before".into(), json!(self.now.to_string()));
            m.insert("resource".into(), json!(scope.aud));
        } else {
            m.insert("expires_in".into(), json!(self.life));
            m.insert("ext_expires_in".into(), json!(self.life));
        }
        m.insert("access_token".into(), json!(access));
    }

    fn respond(&self, m: Map<String, Value>) -> Response {
        Resp::new(200u16)
            .header("Cache-Control", "no-store, no-cache")
            .header("Pragma", "no-cache")
            .header("x-ms-request-id", &self.req.ctx.request_id)
            .json(&Value::Object(m))
            .build()
    }

    /// Tokens for a signed-in user: access token, plus ID token, refresh token and
    /// `client_info` when the scopes and request ask for them.
    async fn user_tokens(&self, user: &User, scope: &Scope, auth: ClientAuth, nonce: Option<&str>, always_refresh: bool) -> Response {
        let key = match self.svc.signing_key().await {
            Ok(k) => k,
            Err(e) => return self.err(500, "server_error", 90033, &e),
        };
        let scp = scope.scp();
        let claims = jwt::access_claims(&AccessToken {
            tenant_id: &self.tid,
            aud: &scope.aud,
            client_id: &self.client_id,
            auth,
            user: Some(user),
            scp: Some(&scp),
            oid: None,
            iat: self.now,
            lifetime: self.life,
        });
        let access = match jwt::sign(&key, &claims) {
            Ok(t) => t,
            Err(e) => return self.err(500, "server_error", 90033, &e),
        };
        let mut m = Map::new();
        if !self.v1 {
            m.insert("scope".into(), json!(scope.granted()));
        }
        self.put_common(&mut m, scope, access);
        if scope.has_oidc("offline_access") || self.v1 || always_refresh {
            let rt = seal(
                RT_PREFIX,
                &Blob {
                    c: self.client_id.clone(),
                    s: scope.raw.clone(),
                    t: self.tenant.to_string(),
                    u: user.upn.clone(),
                    n: None,
                    x: 0,
                    v1: self.v1,
                },
            );
            m.insert("refresh_token".into(), json!(rt));
        }
        if scope.has_oidc("openid") {
            let issuer = format!("{}/{}/v2.0", self.base, self.tid);
            let idc = jwt::id_claims(&issuer, &self.client_id, &self.tid, user, nonce, self.now, self.life);
            match jwt::sign(&key, &idc) {
                Ok(t) => {
                    m.insert("id_token".into(), json!(t));
                }
                Err(e) => return self.err(500, "server_error", 90033, &e),
            }
        }
        if self.p.get("client_info") == Some("1") {
            let ci = json!({"uid": user.oid(), "utid": self.tid});
            m.insert("client_info".into(), json!(b64(ci.to_string().as_bytes())));
        }
        self.respond(m)
    }

    async fn refresh(&self) -> Response {
        let Some(rt) = self.p.nonempty("refresh_token") else {
            return self.missing("refresh_token");
        };
        let Some(blob) = open(RT_PREFIX, rt) else {
            return self.err(400, "invalid_grant", 70000, "The provided value for the input parameter 'refresh_token' is not valid. The refresh token was not issued by this service.");
        };
        let scope = match self.scope_from(Some(&blob)) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let auth = match self.client_auth() {
            Ok(a) => a,
            Err(r) => return r,
        };
        self.user_tokens(&User::new(&blob.u), &scope, auth, None, true).await
    }

    async fn password(&self) -> Response {
        let (Some(user), Some(_password)) = (self.p.nonempty("username"), self.p.nonempty("password")) else {
            return self.missing(if self.p.nonempty("username").is_none() { "username" } else { "password" });
        };
        let scope = match self.scope_from(None) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let auth = match self.client_auth() {
            Ok(a) => a,
            Err(r) => return r,
        };
        self.user_tokens(&User::new(user), &scope, auth, None, false).await
    }

    async fn auth_code(&self) -> Response {
        let Some(code) = self.p.nonempty("code") else {
            return self.missing("code");
        };
        let Some(blob) = open(AC_PREFIX, code) else {
            return self.err(
                400,
                "invalid_grant",
                70000,
                "The provided value for the input parameter 'code' is not valid. The code was not issued by this service.",
            );
        };
        if blob.x != 0 && blob.x < self.now {
            return self.err(400, "invalid_grant", 70008, "The provided authorization code or refresh token has expired.");
        }
        let scope = match self.scope_from(Some(&blob)) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let auth = match self.client_auth() {
            Ok(a) => a,
            Err(r) => return r,
        };
        self.user_tokens(&User::new(&blob.u), &scope, auth, blob.n.as_deref(), false).await
    }

    async fn device_code(&self) -> Response {
        let Some(dc) = self.p.nonempty("device_code") else {
            return self.missing("device_code");
        };
        let Some(blob) = open(DC_PREFIX, dc) else {
            return self.err(400, "invalid_grant", 70000, "The provided value for the input parameter 'device_code' is not valid.");
        };
        if blob.x != 0 && blob.x < self.now {
            return self.err(400, "expired_token", 70019, "Verification code expired. Restart the device flow.");
        }
        // Approved the moment it is issued: no authorization_pending round.
        let scope = match parse_scope(&blob.s, blob.v1, &blob.s) {
            Ok(s) => s,
            Err((e, c, m)) => return self.err(400, e, c, &m),
        };
        let auth = match self.client_auth() {
            Ok(a) => a,
            Err(r) => return r,
        };
        self.user_tokens(&User::new(&blob.u), &scope, auth, None, false).await
    }

    /// On-behalf-of: the `assertion` is the caller's user token; accepted without checking its signature.
    async fn on_behalf_of(&self) -> Response {
        let auth = match self.confidential("urn:ietf:params:oauth:grant-type:jwt-bearer") {
            Ok(a) => a,
            Err(r) => return r,
        };
        let Some(assertion) = self.p.nonempty("assertion") else {
            return self.missing("assertion");
        };
        if self.p.nonempty("requested_token_use") != Some("on_behalf_of") {
            return self.missing("requested_token_use");
        }
        let Some(claims) = jwt::unverified_claims(assertion) else {
            return self.err(400, "invalid_grant", 50027, "JWT token is invalid or malformed.");
        };
        let upn = ["upn", "preferred_username", "unique_name", "email"]
            .iter()
            .find_map(|k| claims.get(*k).and_then(Value::as_str))
            .unwrap_or(DEFAULT_USER);
        let scope = match self.scope_from(None) {
            Ok(s) => s,
            Err(r) => return r,
        };
        self.user_tokens(&User::new(upn), &scope, auth, None, true).await
    }
}

/// `POST /{tenant}/oauth2/v2.0/devicecode` (and the v1 path). The code is approved at once.
pub fn devicecode(req: &Request, base: &str, tenant: &str, v1: bool) -> Response {
    let p = Params::parse(&String::from_utf8_lossy(&req.body));
    let Some(client_id) = p.nonempty("client_id") else {
        return entra_error(req, 400, "invalid_request", 900144, "The request body must contain the following parameter: 'client_id'.");
    };
    let raw = if v1 { p.nonempty("resource") } else { p.nonempty("scope") };
    let Some(raw) = raw else {
        let name = if v1 { "resource" } else { "scope" };
        return entra_error(
            req,
            400,
            "invalid_request",
            900144,
            &format!("The request body must contain the following parameter: '{name}'."),
        );
    };
    if let Err((e, c, m)) = parse_scope(raw, v1, p.get("scope").unwrap_or("")) {
        return entra_error(req, 400, e, c, &m);
    }
    let now = req.ctx.time.unix_timestamp();
    let code =
        seal(DC_PREFIX, &Blob { c: client_id.into(), s: raw.into(), t: tenant.into(), u: DEFAULT_USER.into(), n: None, x: now + 900, v1 });
    let uri = format!("{base}/{tenant}/oauth2/deviceauth");
    let user_code = b64(&crate::util::derive_guid("user-code", &code).into_bytes())[..9].to_ascii_uppercase();
    Resp::new(200u16)
        .header("Cache-Control", "no-store, no-cache")
        .header("Pragma", "no-cache")
        .json(&json!({
            "user_code": user_code,
            "device_code": code,
            "verification_uri": uri,
            "expires_in": 900,
            "interval": 5,
            "message": format!("To sign in, use a web browser to open the page {uri} and enter the code {user_code} to authenticate."),
        }))
        .build()
}
