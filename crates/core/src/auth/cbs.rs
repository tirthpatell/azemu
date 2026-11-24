//! The claims-based security of Service Bus and Event Hubs in strict mode: what `$cbs` `put-token` does with a credential, and what a link
//! attach needs of what the connection holds. Both services answer the same, so the logic is here and each service turns the result
//! into its own error type. The answers are what Azure gave in November 2026 (`docs/strict-auth.md`).

use std::sync::Arc;

use super::claims::{self as grants, ConnClaims, Grant, Holder};
use super::sas::{self, Claim, SasFailure, SasRules};
use super::token::{Aud, TokenFailure};
use super::{Auth, Need, Resource};

/// Which service asks. It decides the audiences an Entra token may have, and the names of the data actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Messaging {
    ServiceBus,
    EventHubs,
}

impl Messaging {
    /// The audiences of an Entra token. Azure's own: `https://servicebus.azure.net` (Service Bus), `https://eventhubs.azure.net` (Event Hubs)
    /// and the namespace's own URL (what Kafka's `OAUTHBEARER` uses). The other service's one is taken too: the findings only tried a
    /// token for a vault (refused), so that is a guess in the lenient direction.
    fn audiences(self, namespace: &str, resolved: &str) -> Vec<Aud> {
        vec![
            Aud::exact("https://servicebus.azure.net"),
            Aud::exact("https://eventhubs.azure.net"),
            Aud::host(namespace),
            Aud::host(resolved),
        ]
    }

    fn resource(self, namespace: &str, path: &str) -> Resource {
        let r = match self {
            Messaging::ServiceBus => Resource::service_bus(namespace),
            Messaging::EventHubs => Resource::event_hubs(namespace),
        };
        if path.is_empty() {
            r
        } else {
            r.at(path)
        }
    }

    fn action(self, c: Claim) -> &'static str {
        match (self, c) {
            (Messaging::ServiceBus, Claim::Send) => "Microsoft.ServiceBus/namespaces/messages/send/action",
            (Messaging::ServiceBus, Claim::Listen) => "Microsoft.ServiceBus/namespaces/messages/receive/action",
            (Messaging::ServiceBus, Claim::Manage) => "Microsoft.ServiceBus/namespaces/messages/manage/action",
            (Messaging::EventHubs, Claim::Send) => "Microsoft.EventHub/namespaces/messages/send/action",
            (Messaging::EventHubs, Claim::Listen) => "Microsoft.EventHub/namespaces/messages/receive/action",
            (Messaging::EventHubs, Claim::Manage) => "Microsoft.EventHub/namespaces/messages/manage/action",
        }
    }

    /// The need of a claim, for the registry.
    pub fn need(self, c: Claim) -> Need {
        Need::data(self.action(c))
    }
}

/// A credential that `put-token` refuses: status `401`, an AMQP condition and the description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: i32,
    pub condition: &'static str,
    pub text: String,
}

const AUTH_FAILED: &str = "com.microsoft:auth-failed";
const UNAUTHORIZED: &str = "amqp:unauthorized-access";

fn now_ts() -> String {
    crate::time::iso8601_secs(crate::time::OffsetDateTime::now_utc())
}

/// ` TrackingId:<guid>_G<n>, SystemTracker:NoSystemTracker, Timestamp:<time>`, the tail Azure puts on its texts.
pub fn tracking() -> String {
    let id = uuid::Uuid::new_v4();
    format!("TrackingId:{id}_G{}, SystemTracker:NoSystemTracker, Timestamp:{}", id.as_bytes()[0] % 32, now_ts())
}

fn refusal(condition: &'static str, text: String) -> Refusal {
    Refusal { status: 401, condition, text }
}

fn expiry_text(secs: i64) -> String {
    let t = crate::time::OffsetDateTime::from_unix_timestamp(secs).unwrap_or(crate::time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

/// The refusal of a SAS token (findings of November 2026, `put-token` on Service Bus and Event Hubs):
/// unknown key name and a rule of another entity: `InvalidSignature: The token has an invalid signature. TrackingId...`;
/// a wrong signature: the same without the tracking part; another entity: `InvalidAudience: The authorization header contains a token with a
/// wrong audience.`; expired: `ExpiredToken: The token is expired. Expiration time: '<time>'`. All `com.microsoft:auth-failed`.
pub fn sas_refusal(f: &SasFailure) -> Refusal {
    match f {
        SasFailure::Malformed => refusal(UNAUTHORIZED, format!("Malformed authorization token. {}", tracking())),
        SasFailure::UnknownKey => refusal(AUTH_FAILED, format!("InvalidSignature: The token has an invalid signature. {}", tracking())),
        SasFailure::BadSignature => refusal(AUTH_FAILED, "InvalidSignature: The token has an invalid signature.".to_string()),
        SasFailure::WrongAudience => {
            refusal(AUTH_FAILED, "InvalidAudience: The authorization header contains a token with a wrong audience.".to_string())
        }
        SasFailure::Expired(se) => {
            refusal(AUTH_FAILED, format!("ExpiredToken: The token is expired. Expiration time: '{}'", expiry_text(*se)))
        }
    }
}

/// The refusal of an Entra token. The wrong audience and the changed signature are what Azure answered; an expired token could not be
/// made on Azure, so its text is a guess in the same family, and so is a token that is not yet valid.
pub fn token_refusal(f: &TokenFailure) -> Refusal {
    match f {
        TokenFailure::Malformed => refusal(UNAUTHORIZED, format!("Malformed authorization token. {}", tracking())),
        TokenFailure::BadSignature => {
            refusal(AUTH_FAILED, format!("InvalidSignature: Invalid authorization token signature. {}", tracking()))
        }
        TokenFailure::WrongAudience(_) => {
            refusal(AUTH_FAILED, format!("InvalidAudience: Invalid authorization token audience. {}", tracking()))
        }
        TokenFailure::Expired(exp) => {
            refusal(AUTH_FAILED, format!("ExpiredToken: The token is expired. Expiration time: '{}'", expiry_text(*exp)))
        }
        TokenFailure::NotYetValid(_) => refusal(AUTH_FAILED, format!("InvalidToken: The token is not valid yet. {}", tracking())),
    }
}

/// What `put-token` is given.
pub struct PutToken<'a> {
    /// The namespace as the connection named it (the first label of the host); an audience may name it.
    pub namespace: &'a str,
    /// The namespace the rules and roles are looked up under: the name above, or the one a config file gave to `default`.
    pub rules_ns: &'a str,
    /// The `name` of the request: the address of the entity the token is for (`sb://ns.servicebus.windows.net/queue`).
    pub name: &'a str,
    pub token: &'a str,
    pub now: i64,
    /// Is this host (as a SAS audience names it) the namespace of the connection? The service's own mapping from a host to a namespace.
    pub host_is_ours: &'a dyn Fn(&str) -> bool,
}

/// The audience path of a `put-token` name: the entity (empty for the namespace).
pub fn name_path(name: &str) -> String {
    let n = name.trim();
    if n.contains("://") {
        sas::audience_path(n)
    } else {
        n.trim_matches('/').to_ascii_lowercase()
    }
}

/// Check a credential and, when it is good, keep it on the connection. `Ok`: answer `202 Accepted`.
pub fn put_token(auth: &Auth, rules: &SasRules, kind: Messaging, claims: &Arc<ConnClaims>, p: &PutToken<'_>) -> Result<(), Refusal> {
    if p.name.len() > grants::MAX_AUDIENCE_BYTES {
        return Err(Refusal {
            status: 403,
            condition: "amqp:resource-limit-exceeded",
            text: format!(
                "The name of the entity is longer than {} bytes, the most azemu keeps (a limit of the emulator).",
                grants::MAX_AUDIENCE_BYTES
            ),
        });
    }
    let path = name_path(p.name);
    let grant = if sas::looks_like_sas(p.token) {
        let Some(t) = sas::parse_token(p.token) else { return Err(sas_refusal(&SasFailure::Malformed)) };
        let chain = sas::chain_of(&path);
        let candidates = rules.candidates(p.rules_ns, &chain);
        let rule = sas::check(&t, &candidates, &path, p.now, p.host_is_ours).map_err(|f| sas_refusal(&f))?;
        Grant {
            audience: path,
            holder: Holder::Rule { name: rule.name, rights: rule.rights },
            // the audience of the token, not of the request, is what the token is good for; the request's audience covers it
            expires: t.good_until(),
        }
    } else {
        let who = auth.authenticate_token(p.token, p.now, &kind.audiences(p.namespace, p.rules_ns)).map_err(|f| token_refusal(&f))?;
        let exp = auth.authority().and_then(|a| a.verify(p.token).ok()).map(|c| c.exp).filter(|e| *e > 0).unwrap_or(p.now + 3600);
        Grant { audience: path, holder: Holder::Principal(who), expires: exp }
    };
    claims.put(grant).map_err(|_| Refusal {
        status: 403,
        condition: "amqp:resource-limit-exceeded",
        text: format!(
            "The connection holds {} credentials, the most azemu keeps for one connection (a limit of the emulator).",
            grants::MAX_GRANTS
        ),
    })?;
    Ok(())
}

/// Does what the connection holds give the claim for the entity at `path`? `res_path` is where the registry looks (`queues/q`,
/// `topics/t/subscriptions/s`, `eventhubs/h`; empty for the namespace).
pub fn has_claim(auth: &Auth, claims: &ConnClaims, kind: Messaging, namespace: &str, path: &str, res_path: &str, claim: Claim) -> bool {
    let res = kind.resource(namespace, res_path);
    let need = kind.need(claim);
    claims.covering(path).iter().any(|h| match h {
        Holder::Rule { rights, .. } => rights.allows(claim),
        Holder::Principal(p) => auth.registry().may(&p.oid, &need, &res),
    })
}

/// The reason a refused attach logs: who held what.
pub fn describe_holders(claims: &ConnClaims, path: &str) -> String {
    let h = claims.covering(path);
    if h.is_empty() {
        return "the connection holds no credential for it".to_string();
    }
    h.iter()
        .map(|h| match h {
            Holder::Rule { name, rights } => format!("the rule '{name}' has {}", rights.names().join(", ")),
            Holder::Principal(p) => format!("principal {} has no role that grants it", p.oid),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `Unauthorized access. 'Send' claim(s) are required to perform this operation. Resource: 'sb://<ns>.servicebus.windows.net/<path>'. TrackingId...`
/// (`amqp:unauthorized-access`, what the detach after a refused attach carries).
pub fn missing_claim_text(claim: Claim, namespace: &str, path: &str) -> String {
    format!(
        "Unauthorized access. '{}' claim(s) are required to perform this operation. Resource: 'sb://{namespace}.servicebus.windows.net/{path}'. {}",
        claim.word(),
        tracking()
    )
}

/// The condition of a refused attach.
pub const UNAUTHORIZED_ACCESS: &str = UNAUTHORIZED;

/// The text a management request answers when the connection may do neither send nor receive (Event Hubs `$management` for a principal
/// with no role: status 401, `amqp:unauthorized-access`).
pub const MANAGEMENT_UNAUTHORIZED: &str = "Attempted to perform an unauthorized operation.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::sas::{make_token, Rights, SasRule, DEV_KEY, ROOT_RULE};

    fn now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn put(rules: &SasRules, claims: &Arc<ConnClaims>, name: &str, token: &str) -> Result<(), Refusal> {
        put_token(
            &Auth::strict(),
            rules,
            Messaging::ServiceBus,
            claims,
            &PutToken { namespace: "ns", rules_ns: "ns", name, token, now: now(), host_is_ours: &|h| h.split('.').next() == Some("ns") },
        )
    }

    #[tokio::test]
    async fn the_refusals_have_azures_condition_and_text() {
        let rules = SasRules::new();
        let claims = ConnClaims::for_conn(2_000_001);
        let n = now();
        let good = make_token("sb://ns.servicebus.windows.net/q", n + 600, ROOT_RULE, DEV_KEY);
        assert!(put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &good).is_ok());
        let expired = make_token("sb://ns.servicebus.windows.net/q", n - 3600, ROOT_RULE, DEV_KEY);
        let r = put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &expired).unwrap_err();
        assert_eq!((r.status, r.condition), (401, "com.microsoft:auth-failed"));
        assert!(r.text.starts_with("ExpiredToken: The token is expired. Expiration time: '"), "{}", r.text);
        let wrong_key = make_token("sb://ns.servicebus.windows.net/q", n + 600, ROOT_RULE, "not-the-key");
        let r = put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &wrong_key).unwrap_err();
        assert_eq!(r.text, "InvalidSignature: The token has an invalid signature.");
        let unknown = make_token("sb://ns.servicebus.windows.net/q", n + 600, "nobody", DEV_KEY);
        let r = put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &unknown).unwrap_err();
        assert!(r.text.starts_with("InvalidSignature: The token has an invalid signature. TrackingId:"), "{}", r.text);
        let other_entity = make_token("sb://ns.servicebus.windows.net/other", n + 600, ROOT_RULE, DEV_KEY);
        let r = put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &other_entity).unwrap_err();
        assert_eq!(r.text, "InvalidAudience: The authorization header contains a token with a wrong audience.");
        // a namespace token and one with http are good
        let ns = make_token("http://ns.servicebus.windows.net/", n + 600, ROOT_RULE, DEV_KEY);
        assert!(put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &ns).is_ok());
    }

    #[tokio::test]
    async fn a_token_for_another_namespace_or_with_a_huge_expiry_is_answered_not_a_panic() {
        let rules = SasRules::new();
        let claims = ConnClaims::for_conn(2_000_010);
        // every namespace has the same development key, so only the host in the audience says which namespace the token is for
        let other = make_token("sb://otherns.servicebus.windows.net/q", now() + 600, ROOT_RULE, DEV_KEY);
        let r = put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &other).unwrap_err();
        assert_eq!(r.text, "InvalidAudience: The authorization header contains a token with a wrong audience.");
        for se in [i64::MAX, i64::MAX - 100] {
            let t = make_token("sb://ns.servicebus.windows.net/q", se, ROOT_RULE, DEV_KEY);
            assert!(put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &t).is_ok());
        }
    }

    #[tokio::test]
    async fn a_connection_that_puts_a_token_for_every_new_name_is_stopped_at_the_limit() {
        let rules = SasRules::new();
        let claims = ConnClaims::for_conn(2_000_012);
        let n = now();
        for i in 0..grants::MAX_GRANTS {
            let name = format!("sb://ns.servicebus.windows.net/q{i}");
            let t = make_token(&name, n + 600, ROOT_RULE, DEV_KEY);
            assert!(put(&rules, &claims, &name, &t).is_ok(), "{i}");
        }
        let name = "sb://ns.servicebus.windows.net/one-more";
        let t = make_token(name, n + 600, ROOT_RULE, DEV_KEY);
        let r = put(&rules, &claims, name, &t).unwrap_err();
        assert_eq!((r.status, r.condition), (403, "amqp:resource-limit-exceeded"));
        assert!(r.text.contains("1000") && r.text.contains("limit of the emulator"), "{}", r.text);
        // a name of any length is not kept
        let long = format!("sb://ns.servicebus.windows.net/{}", "x".repeat(2000));
        let r = put(&rules, &ConnClaims::for_conn(2_000_013), &long, &make_token(&long, n + 600, ROOT_RULE, DEV_KEY)).unwrap_err();
        assert_eq!(r.condition, "amqp:resource-limit-exceeded");
    }

    #[tokio::test]
    async fn a_rule_of_another_entity_is_an_invalid_signature() {
        let rules = SasRules::new();
        rules.declare(
            "ns",
            Some("q1"),
            SasRule::declared("ns", Some("q1"), "q1send", Rights::parse(["Send"]).unwrap(), Some("k".into()), None),
        );
        let claims = ConnClaims::for_conn(2_000_002);
        let t = make_token("sb://ns.servicebus.windows.net/q1", now() + 600, "q1send", "k");
        assert!(put(&rules, &claims, "sb://ns.servicebus.windows.net/q1", &t).is_ok());
        let t = make_token("sb://ns.servicebus.windows.net/q2", now() + 600, "q1send", "k");
        let r = put(&rules, &claims, "sb://ns.servicebus.windows.net/q2", &t).unwrap_err();
        assert!(r.text.starts_with("InvalidSignature: The token has an invalid signature. TrackingId:"), "{}", r.text);
    }

    #[tokio::test]
    async fn rights_are_asked_at_the_attach_not_at_put_token() {
        let rules = SasRules::new();
        rules.declare("ns", None, SasRule::declared("ns", None, "sendonly", Rights::parse(["Send"]).unwrap(), Some("k".into()), None));
        let claims = ConnClaims::for_conn(2_000_003);
        let auth = Auth::strict();
        let t = make_token("sb://ns.servicebus.windows.net/q", now() + 600, "sendonly", "k");
        assert!(
            put(&rules, &claims, "sb://ns.servicebus.windows.net/q", &t).is_ok(),
            "put-token is accepted for a rule that cannot receive"
        );
        assert!(has_claim(&auth, &claims, Messaging::ServiceBus, "ns", "q", "queues/q", Claim::Send));
        assert!(!has_claim(&auth, &claims, Messaging::ServiceBus, "ns", "q", "queues/q", Claim::Listen));
        assert!(!has_claim(&auth, &claims, Messaging::ServiceBus, "ns", "other", "queues/other", Claim::Send), "the token is for q only");
    }

    #[test]
    fn the_missing_claim_text() {
        let t = missing_claim_text(Claim::Send, "ns", "q");
        assert!(t.starts_with("Unauthorized access. 'Send' claim(s) are required to perform this operation. Resource: 'sb://ns.servicebus.windows.net/q'. TrackingId:"), "{t}");
    }
}
