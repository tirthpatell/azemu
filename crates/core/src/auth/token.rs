//! Bearer tokens in strict mode: who issued them, what they say, and the ways they can be wrong.
//!
//! The services do not depend on the identity crate. Core holds a [`TokenAuthority`] that the program puts together hands over (the
//! identity service implements it), the way event sinks are handed over.

use crate::Request;

/// What a token that passed the signature check says.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Claims {
    /// Object id of the caller (lower case).
    pub oid: String,
    pub tid: String,
    /// The client id (`appid` / `azp`), when the token has one.
    pub appid: String,
    pub aud: String,
    pub iss: String,
    /// Unix seconds; 0 when the token has none.
    pub exp: i64,
    pub nbf: i64,
    pub upn: Option<String>,
}

/// Why the signature check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Not three dot-separated parts of base64url, or the parts are not JSON, or the header names no usable algorithm.
    Malformed,
    /// The signature does not verify with the key (also: the token names an algorithm other than RS256, or a key this authority does not have).
    BadSignature,
}

/// Checks tokens and answers questions about principals. The identity service implements it.
pub trait TokenAuthority: Send + Sync {
    /// Verify the signature and read the claims. The times and the audience are checked by core.
    fn verify(&self, token: &str) -> Result<Claims, Fault>;
    /// The object id of the service principal of a client id (what its tokens carry as `oid`).
    fn object_id_of_client(&self, client_id: &str) -> String;
    /// The default managed identity: its client id and its object id.
    fn managed_identity(&self) -> (String, String);
    /// The tenant id the tokens are issued for when the client names none.
    fn default_tenant(&self) -> String;
}

/// Why a token was refused. A service turns each into its own answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenFailure {
    Malformed,
    BadSignature,
    /// `exp` (Unix seconds).
    Expired(i64),
    /// `nbf` (Unix seconds).
    NotYetValid(i64),
    /// The audience the token has.
    WrongAudience(String),
}

/// An audience a service accepts.
///
/// Each service writes down the audiences it takes (`docs/strict-auth.md`, "Audiences"): its own resource (`Exact`: `https://storage.azure.com`)
/// and the URL of the account, vault, store or namespace the request is for (`Host`). A URL is accepted only when the *whole* host is one of the
/// forms the service is known by: the name of the resource, then one of the service's own domains. A token made for `https://acct1.vault.azure.net`
/// is not good for the storage account `acct1`, and neither is one for `https://acct1.evil.example`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aud {
    /// This resource, as a URL or an id (case and a trailing slash do not matter): `https://storage.azure.com`.
    Exact(String),
    /// The URL of one resource: its host is `{name}.{domain}` for one of the `domains`, whatever the scheme, port and path. A domain that starts
    /// with `*.` also takes any labels in front of it (`*.eventgrid.azure.net`: `topic.eastus-1.eventgrid.azure.net`).
    Host { name: String, domains: Vec<String> },
}

/// The domains a Service Bus or Event Hubs namespace is reached under: the Azure clouds' and the emulator's own (`{ns}.servicebus.localhost`,
/// `{ns}.eventhub.localhost`, `{ns}.eventhubs.localhost`). Also what a Kafka client names, as it asks for a token for the bootstrap host.
pub const MESSAGING_DOMAINS: &[&str] = &[
    "servicebus.windows.net",
    "servicebus.chinacloudapi.cn",
    "servicebus.usgovcloudapi.net",
    "servicebus.cloudapi.de",
    "servicebus.windows.net.localhost",
    "servicebus.localhost",
    "eventhub.localhost",
    "eventhubs.localhost",
];

fn norm_aud(a: &str) -> String {
    a.trim().trim_end_matches('/').to_ascii_lowercase()
}

impl Aud {
    pub fn exact(s: &str) -> Aud {
        Aud::Exact(s.to_string())
    }

    /// The URL of the resource `name` under one of `domains` (see [`Aud::Host`]).
    pub fn host_in(name: &str, domains: &[&str]) -> Aud {
        Aud::Host { name: name.to_ascii_lowercase(), domains: domains.iter().map(|d| d.to_ascii_lowercase()).collect() }
    }

    /// The URL of the Service Bus or Event Hubs namespace `name` ([`MESSAGING_DOMAINS`]).
    pub fn host(name: &str) -> Aud {
        Aud::host_in(name, MESSAGING_DOMAINS)
    }

    fn accepts(&self, aud: &str) -> bool {
        let a = norm_aud(aud);
        match self {
            Aud::Exact(e) => norm_aud(e) == a,
            Aud::Host { name, domains } => {
                let rest = a.split_once("://").map(|(_, r)| r).unwrap_or(&a);
                // what follows the host: a port, a path (an audience is a URL, and the emulator's own carry the port)
                let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
                let host = host.strip_suffix('.').unwrap_or(host);
                let Some(after) = host.strip_prefix(name.as_str()).and_then(|r| r.strip_prefix('.')) else { return false };
                domains.iter().any(|d| match d.strip_prefix("*.") {
                    Some(tail) => after == tail || after.ends_with(&format!(".{tail}")),
                    None => after == d,
                })
            }
        }
    }
}

/// Who is calling.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Principal {
    /// Object id, lower case.
    pub oid: String,
    pub tid: String,
    pub appid: String,
    pub upn: Option<String>,
}

impl Principal {
    /// `appid=<client id>;oid=<object id>;iss=https://sts.windows.net/<tenant>/`, the caller text Key Vault shows.
    pub fn caller(&self) -> String {
        format!("appid={};oid={};iss=https://sts.windows.net/{}/", self.appid, self.oid, self.tid)
    }
}

/// The token of an `Authorization: Bearer <token>` header. `None` when there is no such header or the token is empty.
pub fn bearer_token(req: &Request) -> Option<&str> {
    let h = req.header("authorization")?.trim();
    let (scheme, rest) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let t = rest.trim();
    (!t.is_empty()).then_some(t)
}

/// Check a token against an authority: signature, then the times, then the audience (in that order, as Entra-protected services do).
pub fn check(authority: &dyn TokenAuthority, token: &str, now_unix: i64, auds: &[Aud]) -> Result<Principal, TokenFailure> {
    let claims = authority.verify(token).map_err(|f| match f {
        Fault::Malformed => TokenFailure::Malformed,
        Fault::BadSignature => TokenFailure::BadSignature,
    })?;
    if claims.exp != 0 && claims.exp <= now_unix {
        return Err(TokenFailure::Expired(claims.exp));
    }
    if claims.nbf != 0 && claims.nbf > now_unix {
        return Err(TokenFailure::NotYetValid(claims.nbf));
    }
    if !auds.iter().any(|a| a.accepts(&claims.aud)) {
        return Err(TokenFailure::WrongAudience(claims.aud));
    }
    Ok(Principal { oid: claims.oid.to_ascii_lowercase(), tid: claims.tid, appid: claims.appid, upn: claims.upn })
}

/// A token authority for tests of the service crates, which cannot depend on the identity crate: tokens are
/// `header.payload.signature` where the payload is the claims as JSON and the signature is exactly [`TestAuthority::SIGNATURE`].
#[cfg(feature = "testing")]
pub struct TestAuthority;

#[cfg(feature = "testing")]
impl TestAuthority {
    pub const SIGNATURE: &'static str = "dGVzdC1zaWduYXR1cmU";

    /// A token for `oid` with the audience `aud`, valid for an hour from now.
    pub fn token(oid: &str, aud: &str) -> String {
        let now = crate::time::OffsetDateTime::now_utc().unix_timestamp();
        Self::token_with(oid, aud, now - 10, now + 3600)
    }

    pub fn token_with(oid: &str, aud: &str, nbf: i64, exp: i64) -> String {
        use base64::Engine;
        let b = |v: &serde_json::Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap_or_default());
        let header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
        let claims = serde_json::json!({"oid": oid, "tid": "t-0", "appid": format!("client-{oid}"), "aud": aud, "iss": "https://sts.windows.net/t-0/", "nbf": nbf, "exp": exp});
        format!("{}.{}.{}", b(&header), b(&claims), Self::SIGNATURE)
    }
}

#[cfg(feature = "testing")]
impl TokenAuthority for TestAuthority {
    fn verify(&self, token: &str) -> Result<Claims, Fault> {
        use base64::Engine;
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(Fault::Malformed);
        }
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| Fault::Malformed)?;
        let v: serde_json::Value = serde_json::from_slice(&payload).map_err(|_| Fault::Malformed)?;
        if parts[2] != Self::SIGNATURE {
            return Err(Fault::BadSignature);
        }
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        Ok(Claims {
            oid: s("oid"),
            tid: s("tid"),
            appid: s("appid"),
            aud: s("aud"),
            iss: s("iss"),
            exp: n("exp"),
            nbf: n("nbf"),
            upn: None,
        })
    }
    fn object_id_of_client(&self, client_id: &str) -> String {
        format!("oid-of-{client_id}")
    }
    fn managed_identity(&self) -> (String, String) {
        ("mi-client".into(), "mi-oid".into())
    }
    fn default_tenant(&self) -> String {
        "t-0".into()
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;

    fn now() -> i64 {
        crate::time::OffsetDateTime::now_utc().unix_timestamp()
    }

    #[test]
    fn audiences_ignore_case_and_a_trailing_slash_and_match_account_hosts() {
        assert!(Aud::exact("https://storage.azure.com").accepts("https://Storage.Azure.com/"));
        assert!(!Aud::exact("https://storage.azure.com").accepts("https://vault.azure.net"));
        let blob = Aud::host_in("acct", &["blob.core.windows.net", "blob.core.localhost", "blob.localhost"]);
        assert!(blob.accepts("https://acct.blob.core.windows.net"));
        assert!(blob.accepts("HTTPS://ACCT.Blob.Core.Windows.Net:443/path"));
        assert!(blob.accepts("http://acct.blob.localhost:4577/"));
        assert!(blob.accepts("https://acct.blob.core.localhost"));
        assert!(!blob.accepts("https://other.blob.core.windows.net"));
        assert!(!blob.accepts("acct"));
        // the whole host counts, not its first label
        for wrong in [
            "https://acct.vault.azure.net",
            "https://acct.evil.example",
            "https://acct.blob.core.windows.net.evil.example",
            "https://acct.queue.core.windows.net",
            "https://acct.blob.core.windows.net@evil.example",
            "https://evil@acct.blob.core.windows.net",
            "https://x.acct.blob.core.windows.net",
            "https://acct.",
            "https://acct",
            "https://acct.blob",
            "",
        ] {
            assert!(!blob.accepts(wrong), "{wrong}");
        }
        // a domain with a star takes the labels before it (Event Grid puts the region there)
        let eg = Aud::host_in("topic1", &["*.eventgrid.azure.net", "eventgrid.localhost"]);
        assert!(eg.accepts("https://topic1.eastus-1.eventgrid.azure.net/api/events"));
        assert!(eg.accepts("https://topic1.eventgrid.azure.net"));
        assert!(eg.accepts("http://topic1.eventgrid.localhost:4577"));
        assert!(!eg.accepts("https://topic1.eastus-1.eventgrid.azure.net.evil.example"));
        assert!(!eg.accepts("https://topic1.xeventgrid.azure.net"));
        // a messaging namespace
        let ns = Aud::host("Ns1");
        assert!(ns.accepts("https://ns1.servicebus.windows.net"));
        assert!(ns.accepts("https://ns1.eventhub.localhost:4577"));
        assert!(!ns.accepts("https://ns1.vault.azure.net"));
        assert!(!ns.accepts("https://ns1.servicebus.evil.example"));
    }

    #[test]
    fn every_way_a_token_can_be_wrong_has_its_own_failure() {
        let auds = [Aud::exact("https://storage.azure.com")];
        let ok = TestAuthority::token("p1", "https://storage.azure.com");
        assert_eq!(check(&TestAuthority, &ok, now(), &auds).unwrap().oid, "p1");
        assert_eq!(check(&TestAuthority, "garbage", now(), &auds), Err(TokenFailure::Malformed));
        let bad_sig = format!("{}x", ok);
        assert_eq!(check(&TestAuthority, &bad_sig, now(), &auds), Err(TokenFailure::BadSignature));
        let expired = TestAuthority::token_with("p1", "https://storage.azure.com", now() - 7200, now() - 3600);
        assert!(matches!(check(&TestAuthority, &expired, now(), &auds), Err(TokenFailure::Expired(_))));
        let early = TestAuthority::token_with("p1", "https://storage.azure.com", now() + 600, now() + 3600);
        assert!(matches!(check(&TestAuthority, &early, now(), &auds), Err(TokenFailure::NotYetValid(_))));
        let other = TestAuthority::token("p1", "https://vault.azure.net");
        assert_eq!(check(&TestAuthority, &other, now(), &auds), Err(TokenFailure::WrongAudience("https://vault.azure.net".into())));
    }
}
