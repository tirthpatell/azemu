//! Shared access signatures of Service Bus and Event Hubs (strict mode, `docs/strict-auth.md`): authorization rules with their rights and keys,
//! and the check of a `SharedAccessSignature sr=...&sig=...&se=...&skn=...` token against them.
//!
//! A rule belongs to a namespace or to one entity (a queue, a topic, an event hub). Its keys come from one of two places:
//! * Resource Manager, for a namespace it made: the keys `listKeys` hands out are the keys checked here (Resource Manager implements
//!   [`SasSource`] and is asked at every check, so a regenerated key takes effect at once);
//! * the service itself, for every other namespace: the rule `RootManageSharedAccessKey` with the documented development key
//!   [`DEV_KEY`], and the rules declared in the service's config file or through its admin API ([`SasRules::declare`]).
//!
//! What Azure answered for a bad token is in the findings of November 2026: an expired token is accepted for five minutes after its
//! expiry ([`GRACE_SECS`]); an unknown key name, a wrong signature and a rule of another entity are all `InvalidSignature`; a token
//! for another entity is `InvalidAudience`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::storage::sig::hmac_sha256;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use parking_lot::{Mutex, RwLock};

/// The rule every namespace that Resource Manager did not make has.
pub const ROOT_RULE: &str = "RootManageSharedAccessKey";

/// The key of [`ROOT_RULE`] on such a namespace. It is the value Microsoft's own Service Bus and Event Hubs emulators show in their
/// connection string (`SharedAccessKey=SAS_KEY_VALUE`). The key is the text itself, not base64 (a SAS key is signed with as it is written).
pub const DEV_KEY: &str = "SAS_KEY_VALUE";

/// A SAS token is accepted this long after its expiry (found on Azure: 290 s accepted, 300 s refused).
pub const GRACE_SECS: i64 = 300;

/// What a rule may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct Rights {
    pub listen: bool,
    pub send: bool,
    pub manage: bool,
}

/// One right a link or a request needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Send,
    Listen,
    Manage,
}

impl Claim {
    /// The word Azure puts in its text: `'Send' claim(s) are required`.
    pub fn word(self) -> &'static str {
        match self {
            Claim::Send => "Send",
            Claim::Listen => "Listen",
            Claim::Manage => "Manage",
        }
    }
}

impl Rights {
    pub const ALL: Rights = Rights { listen: true, send: true, manage: true };

    /// `["Listen", "Send"]` and the like (what Resource Manager calls `rights`), in any case. An unknown word is the error.
    pub fn parse<S: AsRef<str>>(words: impl IntoIterator<Item = S>) -> Result<Rights, String> {
        let mut r = Rights::default();
        for w in words {
            let word = w.as_ref().trim();
            match word.to_ascii_lowercase().as_str() {
                "listen" => r.listen = true,
                "send" => r.send = true,
                "manage" => r.manage = true,
                _ => return Err(format!("'{word}' is not a right; use Listen, Send or Manage")),
            }
        }
        Ok(r)
    }

    /// `Listen`, `Manage`, `Send` (the order Resource Manager writes them in).
    pub fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.listen {
            v.push("Listen");
        }
        if self.manage {
            v.push("Manage");
        }
        if self.send {
            v.push("Send");
        }
        v
    }

    /// Does a holder of these rights have the claim? `Manage` carries `Send` and `Listen` with it (a guess: on Azure a rule with
    /// `Manage` always has the other two, the portal and Resource Manager insist on it).
    pub fn allows(&self, c: Claim) -> bool {
        match c {
            Claim::Send => self.send || self.manage,
            Claim::Listen => self.listen || self.manage,
            Claim::Manage => self.manage,
        }
    }
}

/// One authorization rule (a shared access policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SasRule {
    pub name: String,
    pub rights: Rights,
    pub primary: String,
    pub secondary: String,
}

impl SasRule {
    /// The rule of a namespace that Resource Manager did not make.
    pub fn root() -> SasRule {
        SasRule { name: ROOT_RULE.to_string(), rights: Rights::ALL, primary: DEV_KEY.to_string(), secondary: DEV_KEY.to_string() }
    }

    /// A rule whose keys are left out gets keys made from where it is, so that the same declaration gives the same keys after a restart.
    pub fn declared(
        namespace: &str,
        entity: Option<&str>,
        name: &str,
        rights: Rights,
        primary: Option<String>,
        secondary: Option<String>,
    ) -> SasRule {
        let made = |which: &str| derived_key(namespace, entity, name, which);
        SasRule {
            name: name.to_string(),
            rights,
            primary: primary.filter(|k| !k.is_empty()).unwrap_or_else(|| made("primary")),
            secondary: secondary.filter(|k| !k.is_empty()).unwrap_or_else(|| made("secondary")),
        }
    }

    /// A rule from the JSON of the config file and the admin API: `name`, `rights` (a list of words), optional `primaryKey` and
    /// `secondaryKey`. Keys may be written in any case (`Name`, `Rights`, `PrimaryKey`).
    pub fn from_json(namespace: &str, entity: Option<&str>, v: &serde_json::Value) -> Result<SasRule, String> {
        let get = |k: &str| v.as_object().and_then(|o| o.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v));
        let name =
            get("name").and_then(|n| n.as_str()).map(str::trim).filter(|n| !n.is_empty()).ok_or("an authorization rule needs a name")?;
        if name.len() > 256 || name.chars().any(|c| c.is_whitespace() || c == '&' || c == '=') {
            return Err(format!("'{name}' is not a valid rule name"));
        }
        let words: Vec<String> = match get("rights") {
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|w| w.as_str().map(str::to_string)).collect(),
            Some(serde_json::Value::String(s)) => s.split([',', ' ']).filter(|w| !w.is_empty()).map(str::to_string).collect(),
            _ => Vec::new(),
        };
        if words.is_empty() {
            return Err(format!("the rule '{name}' needs rights (Listen, Send, Manage)"));
        }
        let rights = Rights::parse(words).map_err(|e| format!("the rule '{name}': {e}"))?;
        let key = |k: &str| get(k).and_then(|x| x.as_str()).map(str::to_string);
        Ok(SasRule::declared(namespace, entity, name, rights, key("primaryKey"), key("secondaryKey")))
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"name": self.name, "rights": self.rights.names(), "primaryKey": self.primary, "secondaryKey": self.secondary})
    }

    fn keys(&self) -> [&str; 2] {
        [&self.primary, &self.secondary]
    }
}

/// `base64(SHA-256("azemu-sas/<namespace>/<entity or ->/<name>/<primary|secondary>"))`.
pub fn derived_key(namespace: &str, entity: Option<&str>, name: &str, which: &str) -> String {
    use sha2::{Digest, Sha256};
    let text = format!(
        "azemu-sas/{}/{}/{}/{which}",
        namespace.to_ascii_lowercase(),
        entity.unwrap_or("-").to_ascii_lowercase(),
        name.to_ascii_lowercase()
    );
    B64.encode(Sha256::digest(text.as_bytes()))
}

/// Where Resource Manager's rules are read from. Implemented by Resource Manager; asked at every check.
pub trait SasSource: Send + Sync {
    /// The rules of a namespace (`entity` is `None`) or of one entity (the path of a queue, topic or event hub, lower case) that
    /// Resource Manager made. `None`: Resource Manager does not know the namespace, and the service's own rules decide.
    fn rules(&self, namespace: &str, entity: Option<&str>) -> Option<Vec<SasRule>>;
}

#[derive(Default)]
struct NsDecl {
    rules: Vec<SasRule>,
    entities: HashMap<String, Vec<SasRule>>,
}

/// The authorization rules of one service (Service Bus or Event Hubs): what was declared, and where Resource Manager's are.
#[derive(Default)]
pub struct SasRules {
    declared: Mutex<HashMap<String, NsDecl>>,
    source: RwLock<Option<Arc<dyn SasSource>>>,
}

fn norm(s: &str) -> String {
    s.trim().trim_matches('/').to_ascii_lowercase()
}

impl SasRules {
    pub fn new() -> SasRules {
        SasRules::default()
    }

    /// Resource Manager's rules (see [`SasSource`]).
    pub fn set_source(&self, s: Arc<dyn SasSource>) {
        *self.source.write() = Some(s);
    }

    /// Declare a rule on a namespace (`entity` is `None`) or on one entity. A rule of the same name is replaced.
    pub fn declare(&self, namespace: &str, entity: Option<&str>, rule: SasRule) {
        let mut g = self.declared.lock();
        let d = g.entry(norm(namespace)).or_default();
        let list = match entity {
            None => &mut d.rules,
            Some(e) => d.entities.entry(norm(e)).or_default(),
        };
        list.retain(|r| !r.name.eq_ignore_ascii_case(&rule.name));
        list.push(rule);
    }

    /// Take a declared rule away. `true` when there was one.
    pub fn remove(&self, namespace: &str, entity: Option<&str>, name: &str) -> bool {
        let mut g = self.declared.lock();
        let Some(d) = g.get_mut(&norm(namespace)) else { return false };
        let list = match entity {
            None => &mut d.rules,
            Some(e) => match d.entities.get_mut(&norm(e)) {
                Some(l) => l,
                None => return false,
            },
        };
        let before = list.len();
        list.retain(|r| !r.name.eq_ignore_ascii_case(name));
        list.len() != before
    }

    /// Forget everything declared (a reset; the config file is read again by the service).
    pub fn clear(&self) {
        self.declared.lock().clear();
    }

    pub fn clear_namespace(&self, namespace: &str) {
        self.declared.lock().remove(&norm(namespace));
    }

    /// The rules that exist at one level: the namespace's own (Resource Manager's when it made the namespace, else the root rule
    /// with the development key), or an entity's. What was declared is added; a declared rule of the same name wins.
    pub fn at(&self, namespace: &str, entity: Option<&str>) -> Vec<SasRule> {
        let ns = norm(namespace);
        let ent = entity.map(norm).filter(|e| !e.is_empty());
        let source = self.source.read().clone();
        let from_arm = source.and_then(|s| s.rules(&ns, ent.as_deref()));
        let mut out: Vec<SasRule> = match (&from_arm, &ent) {
            (Some(rules), _) => rules.clone(),
            (None, None) => vec![SasRule::root()],
            (None, Some(_)) => Vec::new(),
        };
        let g = self.declared.lock();
        if let Some(d) = g.get(&ns) {
            let list = match &ent {
                None => Some(&d.rules),
                Some(e) => d.entities.get(e),
            };
            for r in list.into_iter().flatten() {
                out.retain(|x| !x.name.eq_ignore_ascii_case(&r.name));
                out.push(r.clone());
            }
        }
        out
    }

    /// Every rule that applies to a path: the rules of the entity itself and of what it is inside (a subscription is inside its
    /// topic), then the namespace's. `chain` is the paths from the most specific to the widest, lower case.
    pub fn candidates(&self, namespace: &str, chain: &[String]) -> Vec<SasRule> {
        let mut out = Vec::new();
        for e in chain {
            out.extend(self.at(namespace, Some(e)));
        }
        out.extend(self.at(namespace, None));
        out
    }

    /// Every declared rule, for the admin API: `(namespace, entity, rule)`.
    pub fn declared_list(&self) -> Vec<(String, Option<String>, SasRule)> {
        let g = self.declared.lock();
        let mut v = Vec::new();
        for (ns, d) in g.iter() {
            v.extend(d.rules.iter().map(|r| (ns.clone(), None, r.clone())));
            for (e, rules) in &d.entities {
                v.extend(rules.iter().map(|r| (ns.clone(), Some(e.clone()), r.clone())));
            }
        }
        v.sort_by(|a, b| (&a.0, &a.1, &a.2.name).cmp(&(&b.0, &b.1, &b.2.name)));
        v
    }
}

/// The paths a check looks at for an entity path: the path itself, then each parent (`t/subscriptions/s` is inside `t`). Lower case.
pub fn chain_of(path: &str) -> Vec<String> {
    let p = norm(path);
    if p.is_empty() {
        return Vec::new();
    }
    let first = p.split('/').next().unwrap_or("").to_string();
    if first == p {
        vec![p]
    } else {
        vec![p, first]
    }
}

// ---- the token ----------------------------------------------------------------------------------------------------------------------

/// A parsed `SharedAccessSignature` token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SasToken {
    /// The audience as written in the token (still percent-encoded: what was signed).
    pub sr: String,
    /// The signature, percent-decoded.
    pub sig: String,
    /// Expiry, Unix seconds.
    pub se: i64,
    pub skn: String,
}

impl SasToken {
    /// The last second the token is good for: its expiry and the grace. Saturating: the key is public, so anybody can sign `se=9223372036854775807`.
    pub fn good_until(&self) -> i64 {
        self.se.saturating_add(GRACE_SECS)
    }
}

/// Does the text look like a SAS token? (the scheme word, or the `sr=` pairs without it)
pub fn looks_like_sas(text: &str) -> bool {
    let t = text.trim();
    t.get(..21).is_some_and(|p| p.eq_ignore_ascii_case("SharedAccessSignature")) || (t.contains("sr=") && t.contains("sig="))
}

fn pct_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Read `SharedAccessSignature sr=...&sig=...&se=...&skn=...` (the scheme word is optional). `None` when a part is missing.
pub fn parse_token(text: &str) -> Option<SasToken> {
    let t = text.trim();
    let t = match t.get(..21) {
        Some(p) if p.eq_ignore_ascii_case("SharedAccessSignature") => t[21..].trim(),
        _ => t,
    };
    let (mut sr, mut sig, mut se, mut skn) = (None, None, None, None);
    for pair in t.split('&') {
        let Some((k, v)) = pair.trim().split_once('=') else { continue };
        match k.to_ascii_lowercase().as_str() {
            "sr" => sr = Some(v.to_string()),
            "sig" => sig = Some(pct_decode(v)),
            "se" => se = v.parse::<i64>().ok(),
            "skn" => skn = Some(pct_decode(v)),
            _ => {}
        }
    }
    Some(SasToken { sr: sr?, sig: sig?, se: se?, skn: skn? })
}

/// Why a SAS token is refused. A service turns each into Azure's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SasFailure {
    /// A part is missing or unreadable.
    Malformed,
    /// No rule of that name where the token is used (also: the entity-level rule of another entity). Azure's text has the tracking part.
    UnknownKey,
    /// The rule exists but neither key makes this signature. Azure's text has no tracking part.
    BadSignature,
    /// `sr` does not cover the entity.
    WrongAudience,
    /// The expiry (Unix seconds) is more than [`GRACE_SECS`] past.
    Expired(i64),
}

/// The signature of `sr` (as written in the token) and `se` under a key: `base64(HMAC-SHA256(key, sr + "\n" + se))`.
pub fn sign(sr: &str, se: i64, key: &str) -> String {
    B64.encode(hmac_sha256(key.as_bytes(), format!("{sr}\n{se}").as_bytes()))
}

/// Make a token (what the SDKs do; tests and the Terraform check use it).
pub fn make_token(sr: &str, se: i64, rule: &str, key: &str) -> String {
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    let sr_enc = enc(sr);
    format!("SharedAccessSignature sr={sr_enc}&sig={}&se={se}&skn={}", enc(&sign(&sr_enc, se, key)), enc(rule))
}

/// The path an `sr` names: its host and scheme dropped, percent-decoded, lower case, no outer slashes (`sb://ns.servicebus.windows.net/Q` is `q`).
pub fn audience_path(sr: &str) -> String {
    let d = pct_decode(sr);
    let path = match d.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p).unwrap_or(""),
        // no scheme: `ns.servicebus.windows.net/q` has a host, `q/$management` has none
        None => match d.split_once('/') {
            Some((host, p)) if host.contains('.') => p,
            _ if d.contains('.') && !d.contains('/') => "",
            _ => d.as_str(),
        },
    };
    path.split(['?', '#']).next().unwrap_or("").trim_matches('/').to_ascii_lowercase()
}

/// Does an audience cover an entity? The namespace covers everything; an entity covers itself and what is below it.
pub fn covers(audience: &str, entity: &str) -> bool {
    let a = norm(audience);
    let e = norm(entity);
    a.is_empty() || e == a || e.starts_with(&format!("{a}/"))
}

/// The signatures a token may carry for one key: the one over `sr` as written, and over the usual encodings of what it says (the SDKs
/// differ in what they escape).
fn signatures(t: &SasToken, key: &str) -> Vec<String> {
    let decoded = pct_decode(&t.sr);
    let quote_plus = percent_encoding::utf8_percent_encode(&decoded, percent_encoding::NON_ALPHANUMERIC).to_string();
    let mut v = vec![sign(&t.sr, t.se, key), sign(&quote_plus, t.se, key), sign(&decoded, t.se, key)];
    v.dedup();
    v
}

/// The host an `sr` names, lower case, without a port: `sb://NS.servicebus.windows.net:443/q` is `ns.servicebus.windows.net`. `None` when
/// `sr` names no host (`q/$management`).
pub fn audience_host(sr: &str) -> Option<String> {
    let d = pct_decode(sr);
    let host = match d.split_once("://") {
        Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or(""),
        None => match d.split_once('/') {
            Some((host, _)) if host.contains('.') => host,
            None if d.contains('.') => d.as_str(),
            _ => return None,
        },
    };
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(h, _)| h).unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Check a token for the use of `entity` (a path, lower case or not; empty for the namespace) against the rules that apply.
/// `host_is_ours` says whether the host an `sr` names is the namespace the token is used on (the caller knows how the service maps a host to
/// a namespace); an `sr` with no host is not asked. Azure answers `InvalidAudience` for a token made for another namespace.
/// Order: the key name, the signature, the audience, the expiry (the findings give each refusal, not the order between them).
pub fn check(t: &SasToken, rules: &[SasRule], entity: &str, now: i64, host_is_ours: &dyn Fn(&str) -> bool) -> Result<SasRule, SasFailure> {
    let Some(rule) = rules.iter().find(|r| r.name.eq_ignore_ascii_case(&t.skn)) else { return Err(SasFailure::UnknownKey) };
    let ok = rule.keys().iter().any(|k| !k.is_empty() && signatures(t, k).contains(&t.sig));
    if !ok {
        return Err(SasFailure::BadSignature);
    }
    if !covers(&audience_path(&t.sr), entity) || audience_host(&t.sr).is_some_and(|h| !host_is_ours(&h)) {
        return Err(SasFailure::WrongAudience);
    }
    if t.good_until() <= now {
        return Err(SasFailure::Expired(t.se));
    }
    Ok(rule.clone())
}

// ---- connection strings ------------------------------------------------------------------------------------------------------------

/// The parts of `Endpoint=sb://host/;SharedAccessKeyName=...;SharedAccessKey=...;EntityPath=...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionString {
    pub endpoint: Option<String>,
    pub key_name: Option<String>,
    pub key: Option<String>,
    pub entity_path: Option<String>,
}

impl ConnectionString {
    /// The host of the endpoint (`ns.servicebus.windows.net`), without scheme, port or path.
    pub fn host(&self) -> Option<String> {
        let e = self.endpoint.as_deref()?;
        let rest = e.split_once("://").map(|(_, r)| r).unwrap_or(e);
        Some(rest.split(['/', ':']).next().unwrap_or("").to_string())
    }
}

/// `None` when it is not a connection string of Service Bus or Event Hubs: a part that is not `name=value`, a name that is not one of
/// the known ones (`Endpoint`, `SharedAccessKeyName`, `SharedAccessKey`, `EntityPath`, `UseDevelopmentEmulator`, `TransportType`,
/// `OperationTimeout`, `SharedAccessSignature`), or no `Endpoint`.
pub fn parse_connection_string(s: &str) -> Option<ConnectionString> {
    let mut c = ConnectionString::default();
    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        match k.trim().to_ascii_lowercase().as_str() {
            "endpoint" => c.endpoint = Some(v.trim().to_string()),
            "sharedaccesskeyname" => c.key_name = Some(v.trim().to_string()),
            "sharedaccesskey" => c.key = Some(v.to_string()),
            "entitypath" => c.entity_path = Some(v.trim().to_string()),
            "usedevelopmentemulator" | "transporttype" | "operationtimeout" | "sharedaccesssignature" | "authentication" => {}
            _ => return None,
        }
    }
    c.endpoint.is_some().then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn rules() -> Vec<SasRule> {
        vec![
            SasRule::root(),
            SasRule::declared("ns", None, "sendonly", Rights::parse(["Send"]).unwrap(), Some("send-key".into()), Some("send-key-2".into())),
        ]
    }

    fn ours(host: &str) -> bool {
        host.split('.').next() == Some("ns")
    }

    fn token(sr: &str, se: i64, rule: &str, key: &str) -> SasToken {
        parse_token(&make_token(sr, se, rule, key)).unwrap()
    }

    #[test]
    fn a_token_is_read_back_and_checked_with_either_key() {
        let r = rules();
        let t = token("sb://ns.servicebus.windows.net/q", NOW + 3600, "sendonly", "send-key");
        assert_eq!(t.skn, "sendonly");
        assert_eq!(check(&t, &r, "q", NOW, &ours).unwrap().name, "sendonly");
        let t2 = token("sb://ns.servicebus.windows.net/q", NOW + 3600, "sendonly", "send-key-2");
        assert!(check(&t2, &r, "q", NOW, &ours).is_ok());
        let root = token("sb://ns.servicebus.windows.net/q", NOW + 3600, ROOT_RULE, DEV_KEY);
        assert!(check(&root, &r, "q", NOW, &ours).is_ok());
    }

    #[test]
    fn the_key_name_and_the_signature_are_told_apart() {
        let r = rules();
        let wrong = token("sb://ns/q", NOW + 3600, "sendonly", "other-key");
        assert_eq!(check(&wrong, &r, "q", NOW, &ours), Err(SasFailure::BadSignature));
        // the name of the send rule with the root key's signature is a bad signature, not an unknown key
        let mixed = token("sb://ns/q", NOW + 3600, "sendonly", DEV_KEY);
        assert_eq!(check(&mixed, &r, "q", NOW, &ours), Err(SasFailure::BadSignature));
        let unknown = token("sb://ns/q", NOW + 3600, "nobody", DEV_KEY);
        assert_eq!(check(&unknown, &r, "q", NOW, &ours), Err(SasFailure::UnknownKey));
    }

    #[test]
    fn the_audience_must_cover_the_entity() {
        let r = rules();
        let ns = token("sb://ns/", NOW + 3600, ROOT_RULE, DEV_KEY);
        assert!(check(&ns, &r, "q", NOW, &ours).is_ok());
        assert!(check(&ns, &r, "t/subscriptions/s", NOW, &ours).is_ok());
        let q = token("sb://ns/q", NOW + 3600, ROOT_RULE, DEV_KEY);
        assert!(check(&q, &r, "q", NOW, &ours).is_ok());
        assert!(check(&q, &r, "q/$management", NOW, &ours).is_ok());
        assert!(check(&q, &r, "q/$deadletterqueue", NOW, &ours).is_ok());
        assert_eq!(check(&q, &r, "other", NOW, &ours), Err(SasFailure::WrongAudience));
        assert_eq!(check(&q, &r, "queue2", NOW, &ours), Err(SasFailure::WrongAudience), "a prefix of the name is not the entity");
        assert_eq!(check(&q, &r, "", NOW, &ours), Err(SasFailure::WrongAudience), "an entity token does not cover the namespace");
        // http instead of sb, upper case, a port
        let h = token("http://NS.servicebus.windows.net:443/Q", NOW + 3600, ROOT_RULE, DEV_KEY);
        assert!(check(&h, &r, "q", NOW, &ours).is_ok());
    }

    #[test]
    fn a_token_is_good_for_five_minutes_after_its_expiry() {
        // found on Azure: 290 s after expiry accepted, 300 s refused
        let r = rules();
        let t = token("sb://ns/q", NOW - 290, ROOT_RULE, DEV_KEY);
        assert!(check(&t, &r, "q", NOW, &ours).is_ok());
        let t = token("sb://ns/q", NOW - 299, ROOT_RULE, DEV_KEY);
        assert!(check(&t, &r, "q", NOW, &ours).is_ok());
        let t = token("sb://ns/q", NOW - 300, ROOT_RULE, DEV_KEY);
        assert_eq!(check(&t, &r, "q", NOW, &ours), Err(SasFailure::Expired(NOW - 300)));
        let t = token("sb://ns/q", NOW - 3600, ROOT_RULE, DEV_KEY);
        assert_eq!(check(&t, &r, "q", NOW, &ours), Err(SasFailure::Expired(NOW - 3600)));
    }

    #[test]
    fn an_expiry_at_the_end_of_time_is_a_good_token_not_a_panic() {
        let r = rules();
        let t = token("sb://ns/q", i64::MAX, ROOT_RULE, DEV_KEY);
        assert!(check(&t, &r, "q", NOW, &ours).is_ok());
        assert_eq!(t.good_until(), i64::MAX);
        let t = token("sb://ns/q", i64::MIN, ROOT_RULE, DEV_KEY);
        assert_eq!(check(&t, &r, "q", NOW, &ours), Err(SasFailure::Expired(i64::MIN)));
    }

    #[test]
    fn a_token_for_another_namespace_is_a_wrong_audience() {
        let r = rules();
        // the development key is the same in every namespace, so the host of `sr` is the only thing that tells them apart
        for (sr, good) in [
            ("sb://ns.servicebus.windows.net/q", true),
            ("sb://NS.servicebus.windows.net:443/Q", true),
            ("http://ns.servicebus.localhost:5672/q", true),
            ("sb://ns/q", true),
            ("ns.servicebus.windows.net/q", true),
            ("q", true),
            ("sb://otherns.servicebus.windows.net/q", false),
            ("sb://otherns/q", false),
            ("sb://evil@otherns.servicebus.windows.net/q", false),
            ("otherns.servicebus.windows.net/q", false),
        ] {
            let t = token(sr, NOW + 3600, ROOT_RULE, DEV_KEY);
            let want = if good { Ok(()) } else { Err(SasFailure::WrongAudience) };
            assert_eq!(check(&t, &r, "q", NOW, &ours).map(|_| ()), want, "{sr}");
        }
        assert_eq!(audience_host("sb://A.b.c:5671/q?x=1").as_deref(), Some("a.b.c"));
        assert_eq!(audience_host("q/$management"), None);
    }

    #[test]
    fn rights_and_claims() {
        let send = Rights::parse(["send"]).unwrap();
        assert!(send.allows(Claim::Send) && !send.allows(Claim::Listen) && !send.allows(Claim::Manage));
        let manage = Rights::parse(["Manage"]).unwrap();
        assert!(manage.allows(Claim::Send) && manage.allows(Claim::Listen) && manage.allows(Claim::Manage));
        assert!(Rights::parse(["Read"]).is_err());
        assert_eq!(Rights::ALL.names(), ["Listen", "Manage", "Send"]);
    }

    #[test]
    fn garbage_is_malformed() {
        assert!(parse_token("garbage").is_none());
        assert!(parse_token("SharedAccessSignature sr=x&sig=y&se=notanumber&skn=k").is_none());
        assert!(parse_token("SharedAccessSignature sr=x&sig=y&se=1").is_none());
        assert!(looks_like_sas("SharedAccessSignature sr=x"));
        assert!(!looks_like_sas("eyJhbGciOi.e30.sig"));
    }

    #[test]
    fn declared_rules_stand_with_the_root_rule_and_win_by_name() {
        let s = SasRules::new();
        assert_eq!(s.at("ns", None).len(), 1, "the root rule");
        s.declare("NS", None, SasRule::declared("ns", None, "listen", Rights::parse(["Listen"]).unwrap(), None, None));
        s.declare("ns", Some("Q"), SasRule::declared("ns", Some("q"), "qsend", Rights::parse(["Send"]).unwrap(), Some("k".into()), None));
        assert_eq!(s.at("ns", None).len(), 2);
        assert_eq!(s.at("ns", Some("q")).len(), 1);
        let c = s.candidates("ns", &chain_of("Q/$management"));
        assert!(c.iter().any(|r| r.name == "qsend") && c.iter().any(|r| r.name == ROOT_RULE));
        // the root rule can be replaced by declaring it again
        s.declare("ns", None, SasRule::declared("ns", None, ROOT_RULE, Rights::ALL, Some("mine".into()), None));
        let root: Vec<_> = s.at("ns", None).into_iter().filter(|r| r.name == ROOT_RULE).collect();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].primary, "mine");
        assert!(s.remove("ns", Some("q"), "QSEND"));
        assert!(s.at("ns", Some("q")).is_empty());
    }

    #[test]
    fn rules_from_resource_manager_replace_the_root_rule() {
        struct Arm;
        impl SasSource for Arm {
            fn rules(&self, ns: &str, entity: Option<&str>) -> Option<Vec<SasRule>> {
                (ns == "made").then(|| match entity {
                    None => vec![SasRule::declared("made", None, "root", Rights::ALL, Some("arm-key".into()), None)],
                    Some(_) => vec![],
                })
            }
        }
        let s = SasRules::new();
        s.set_source(Arc::new(Arm));
        assert_eq!(s.at("made", None)[0].primary, "arm-key");
        assert_eq!(s.at("made", None).len(), 1, "no development key on a namespace Resource Manager made");
        assert_eq!(s.at("other", None)[0].primary, DEV_KEY);
    }

    #[test]
    fn connection_strings() {
        let c = parse_connection_string("Endpoint=sb://ns.servicebus.windows.net/;SharedAccessKeyName=k;SharedAccessKey=a=b=;EntityPath=q")
            .unwrap();
        assert_eq!(c.key.as_deref(), Some("a=b="));
        assert_eq!(c.host().as_deref(), Some("ns.servicebus.windows.net"));
        assert_eq!(c.entity_path.as_deref(), Some("q"));
        assert!(parse_connection_string("not a connection string").is_none());
        assert!(parse_connection_string("Endpoint=sb://x/;Surprise=1").is_none());
        assert!(parse_connection_string("SharedAccessKey=x").is_none());
        let dev = parse_connection_string("Endpoint=sb://localhost;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=SAS_KEY_VALUE;UseDevelopmentEmulator=true;").unwrap();
        assert_eq!(dev.host().as_deref(), Some("localhost"));
    }

    #[test]
    fn json_rules() {
        let v = serde_json::json!({"Name": "r", "Rights": ["Send", "Listen"], "PrimaryKey": "p"});
        let r = SasRule::from_json("ns", None, &v).unwrap();
        assert_eq!(r.primary, "p");
        assert_ne!(r.secondary, "p");
        assert!(r.rights.send && r.rights.listen && !r.rights.manage);
        assert!(SasRule::from_json("ns", None, &serde_json::json!({"name": "r"})).is_err());
        assert!(SasRule::from_json("ns", None, &serde_json::json!({"name": "r", "rights": ["Fly"]})).is_err());
        // the same declaration gives the same keys
        assert_eq!(
            SasRule::from_json("ns", None, &serde_json::json!({"name": "x", "rights": ["Send"]})).unwrap(),
            SasRule::from_json("ns", None, &serde_json::json!({"name": "x", "rights": ["Send"]})).unwrap()
        );
    }
}
