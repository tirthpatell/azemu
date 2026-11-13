//! Token contents: the claims Entra puts in access tokens and ID tokens, and the
//! compact RS256 serialization.

use serde_json::{json, Map, Value};

use crate::keys::SigningKey;
use crate::util::{b64, derive_guid, unb64};

/// Name of the user the interactive flows (device code, authorization code) sign in.
pub const DEFAULT_USER: &str = "azemu.user@azemu.onmicrosoft.com";

/// How the caller proved who it is; becomes `appidacr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClientAuth {
    /// Public client, no secret: "0".
    Public,
    /// Client secret: "1".
    Secret,
    /// Certificate or federated assertion, managed identity: "2".
    Assertion,
}

impl ClientAuth {
    fn acr(self) -> &'static str {
        match self {
            ClientAuth::Public => "0",
            ClientAuth::Secret => "1",
            ClientAuth::Assertion => "2",
        }
    }
}

/// A signed-in user, identified by the user name the client sent.
#[derive(Clone, Debug)]
pub struct User {
    pub upn: String,
}

impl User {
    pub fn new(upn: &str) -> User {
        User { upn: upn.trim().to_string() }
    }
    pub fn oid(&self) -> String {
        derive_guid("user-oid", &self.upn.to_ascii_lowercase())
    }
    /// Pairwise subject: differs per application, as in Entra.
    pub fn sub(&self, client_id: &str) -> String {
        derive_guid("user-sub", &format!("{}\n{}", client_id.to_ascii_lowercase(), self.upn.to_ascii_lowercase()))
    }
    pub fn given_name(&self) -> String {
        self.upn.split('@').next().unwrap_or("user").to_string()
    }
}

/// The object id of the service principal behind a client id. Stable per client id.
pub fn app_oid(client_id: &str) -> String {
    derive_guid("app-oid", &client_id.to_ascii_lowercase())
}

pub struct AccessToken<'a> {
    pub tenant_id: &'a str,
    pub aud: &'a str,
    pub client_id: &'a str,
    pub auth: ClientAuth,
    /// `None` for an app-only token (client credentials, managed identity).
    pub user: Option<&'a User>,
    /// Space separated permission names (user tokens).
    pub scp: Option<&'a str>,
    /// Overrides the derived object id (managed identity requested by object id).
    pub oid: Option<&'a str>,
    pub iat: i64,
    pub lifetime: i64,
}

/// Version 1.0 claims, which is what Entra issues for Azure resources (Storage,
/// Key Vault, ARM, ...) whichever endpoint the client used.
pub fn access_claims(t: &AccessToken) -> Value {
    let iss = format!("https://sts.windows.net/{}/", t.tenant_id);
    let mut m = Map::new();
    m.insert("aud".into(), json!(t.aud));
    m.insert("iss".into(), json!(iss));
    m.insert("iat".into(), json!(t.iat));
    m.insert("nbf".into(), json!(t.iat));
    m.insert("exp".into(), json!(t.iat + t.lifetime));
    match t.user {
        None => {
            let oid = t.oid.map(str::to_string).unwrap_or_else(|| app_oid(t.client_id));
            m.insert("appid".into(), json!(t.client_id));
            m.insert("appidacr".into(), json!(t.auth.acr()));
            m.insert("idp".into(), json!(iss));
            m.insert("idtyp".into(), json!("app"));
            m.insert("oid".into(), json!(oid));
            m.insert("sub".into(), json!(oid));
            m.insert("tid".into(), json!(t.tenant_id));
            m.insert("ver".into(), json!("1.0"));
        }
        Some(u) => {
            m.insert("acr".into(), json!("1"));
            m.insert("amr".into(), json!(["pwd"]));
            m.insert("appid".into(), json!(t.client_id));
            m.insert("appidacr".into(), json!(t.auth.acr()));
            m.insert("family_name".into(), json!("Azemu"));
            m.insert("given_name".into(), json!(u.given_name()));
            m.insert("idp".into(), json!(iss));
            m.insert("ipaddr".into(), json!("127.0.0.1"));
            m.insert("name".into(), json!(u.given_name()));
            m.insert("oid".into(), json!(u.oid()));
            m.insert("scp".into(), json!(t.scp.unwrap_or("user_impersonation")));
            m.insert("sub".into(), json!(u.sub(t.client_id)));
            m.insert("tid".into(), json!(t.tenant_id));
            m.insert("unique_name".into(), json!(u.upn));
            m.insert("upn".into(), json!(u.upn));
            m.insert("ver".into(), json!("1.0"));
        }
    }
    Value::Object(m)
}

/// Version 2.0 ID token claims.
pub fn id_claims(issuer: &str, client_id: &str, tenant_id: &str, user: &User, nonce: Option<&str>, iat: i64, lifetime: i64) -> Value {
    let mut m = Map::new();
    m.insert("aud".into(), json!(client_id));
    m.insert("iss".into(), json!(issuer));
    m.insert("iat".into(), json!(iat));
    m.insert("nbf".into(), json!(iat));
    m.insert("exp".into(), json!(iat + lifetime));
    m.insert("name".into(), json!(user.given_name()));
    m.insert("oid".into(), json!(user.oid()));
    m.insert("preferred_username".into(), json!(user.upn));
    m.insert("sub".into(), json!(user.sub(client_id)));
    m.insert("tid".into(), json!(tenant_id));
    m.insert("ver".into(), json!("2.0"));
    if let Some(n) = nonce {
        m.insert("nonce".into(), json!(n));
    }
    Value::Object(m)
}

/// Header, claims and RS256 signature in compact form.
pub fn sign(key: &SigningKey, claims: &Value) -> Result<String, String> {
    let header = json!({"typ": "JWT", "alg": "RS256", "x5t": key.kid, "kid": key.kid});
    let h = serde_json::to_vec(&header).map_err(|e| e.to_string())?;
    let c = serde_json::to_vec(claims).map_err(|e| e.to_string())?;
    let signing_input = format!("{}.{}", b64(&h), b64(&c));
    let sig = key.sign_rs256(signing_input.as_bytes())?;
    Ok(format!("{signing_input}.{}", b64(&sig)))
}

/// Claims of a JWT without checking the signature (used for assertions we accept as is).
pub fn unverified_claims(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    serde_json::from_slice(&unb64(payload)?).ok()
}
