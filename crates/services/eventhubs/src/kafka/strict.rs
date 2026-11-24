//! Strict authorisation of the Kafka endpoint (`docs/strict-auth.md`): what a connection may do depends on how it logged in.
//!
//! `SASL PLAIN` with the user `$ConnectionString` and a connection string that carries a rule's key; or `SASL OAUTHBEARER` with an Entra token.
//! What Azure answered (November 2026): a wrong key or an unknown policy name is "Invalid SASL PLAIN password."; another user is "Invalid SASL PLAIN
//! user name."; a password that is no connection string has its own text; a made-up bearer token "Invalid SASL OAUTHBEARER message.", a token whose
//! signature was changed `InvalidSignature: ...`, one for another audience "Invalid URI: ...". The connection is closed after each. After the login a
//! produce needs `Send` and a fetch, an offset fetch or commit needs `Listen`, and the answer to a missing right is `TOPIC_AUTHORIZATION_FAILED` (29)
//! for that topic. A principal with no role at all has its connection closed at the coordinator lookup.

use azemu_core::auth::cbs::{Messaging, Refusal};
use azemu_core::auth::sas::{self, Claim, Rights};
use azemu_core::auth::token::Aud;
use azemu_core::auth::{Auth, Principal, Resource};

use super::handlers::{Broker, Conn};

/// How a connection logged in.
#[derive(Debug, Clone, Default)]
pub enum Access {
    /// No successful login yet (strict mode only: every request but the login is answered by closing the connection).
    #[default]
    Anonymous,
    /// A connection string: the rights of its rule; `entity` is the event hub of an entity-level rule (it may use that hub only).
    Rule { rights: Rights, entity: Option<String> },
    /// An Entra token.
    Principal(Principal),
}

fn unix_now() -> i64 {
    azemu_core::time::OffsetDateTime::now_utc().unix_timestamp()
}

/// The token of an `OAUTHBEARER` first message (`n,,\x01auth=Bearer <token>\x01\x01`).
pub fn bearer_of(msg: &str) -> Option<String> {
    let i = msg.to_ascii_lowercase().find("auth=bearer ")?;
    let rest = &msg[i + "auth=bearer ".len()..];
    Some(rest.split('\u{1}').next().unwrap_or("").trim().to_string()).filter(|t| !t.is_empty())
}

impl Broker {
    /// The handle, when the mode is on.
    pub(crate) fn strict(&self) -> Option<Auth> {
        let a = self.shared.auth();
        a.is_strict().then_some(a)
    }

    /// The namespace the rules and roles are looked up under. Taken once per connection, before any handler holds the state lock (the lock
    /// is not re-entrant), by [`Broker::handle`].
    pub(crate) fn rules_ns(&self, c: &Conn) -> String {
        // before the login the namespace is not known yet (the connection string names it): not kept until then
        if matches!(c.access, Access::Anonymous) {
            return self.shared.state.lock().resolve(&c.namespace());
        }
        c.rules_ns.get_or_init(|| self.shared.state.lock().resolve(&c.namespace())).clone()
    }

    /// `SASL PLAIN` in strict mode: `Err` is the text of the failure.
    pub(crate) fn login_plain(&self, c: &mut Conn, user: &str, password: &str) -> Result<(), String> {
        if user != "$ConnectionString" {
            return Err("Invalid SASL PLAIN user name.".to_string());
        }
        const BAD_SHAPE: &str = "The Service Bus connection string is not of the expected format. Either there are unexpected properties within the string or the format is incorrect. Please check the string before trying again.";
        let Some(cs) = sas::parse_connection_string(password) else { return Err(BAD_SHAPE.to_string()) };
        let (Some(name), Some(key)) = (cs.key_name.clone(), cs.key.clone()) else { return Err(BAD_SHAPE.to_string()) };
        // the namespace the string names; the TLS server name, when the client used one, must agree
        let named = cs.host().map(|h| crate::amqp_conn::namespace_from_hostname(Some(&h))).unwrap_or_default();
        let sni =
            c.sni.as_deref().map(|s| crate::amqp_conn::namespace_from_hostname(Some(s))).filter(|n| n != crate::state::DEFAULT_NAMESPACE);
        if let Some(s) = sni {
            if !named.is_empty() && named != crate::state::DEFAULT_NAMESPACE && named != s {
                return Err(format!("Invalid tenant name '{named}'"));
            }
        }
        let rns = self.rules_ns(c);
        let entity = cs.entity_path.as_deref().map(|e| e.trim_matches('/').to_ascii_lowercase()).filter(|e| !e.is_empty());
        let mut candidates = self.shared.sas.at(&rns, None).into_iter().map(|r| (r, None)).collect::<Vec<_>>();
        if let Some(e) = &entity {
            candidates.extend(self.shared.sas.at(&rns, Some(e)).into_iter().map(|r| (r, Some(e.clone()))));
        }
        let found = candidates.iter().find(|(r, _)| r.name.eq_ignore_ascii_case(&name) && (r.primary == key || r.secondary == key));
        match found {
            Some((rule, e)) => {
                c.access = Access::Rule { rights: rule.rights, entity: e.clone() };
                Ok(())
            }
            None => Err("Invalid SASL PLAIN password.".to_string()),
        }
    }

    /// `SASL OAUTHBEARER` in strict mode.
    pub(crate) fn login_bearer(&self, auth: &Auth, c: &mut Conn, message: &str) -> Result<(), String> {
        let Some(token) = bearer_of(message) else { return Err("Invalid SASL OAUTHBEARER message.".to_string()) };
        let ns = c.namespace();
        let rns = self.rules_ns(c);
        let auds = [Aud::exact("https://eventhubs.azure.net"), Aud::exact("https://servicebus.azure.net"), Aud::host(&ns), Aud::host(&rns)];
        match auth.authenticate_token(&token, unix_now(), &auds) {
            Ok(p) => {
                c.access = Access::Principal(p);
                Ok(())
            }
            Err(azemu_core::auth::TokenFailure::Malformed) => Err("Invalid SASL OAUTHBEARER message.".to_string()),
            Err(azemu_core::auth::TokenFailure::WrongAudience(_)) => {
                Err("Invalid URI: The format of the URI could not be determined.".to_string())
            }
            Err(f) => {
                let r: Refusal = azemu_core::auth::cbs::token_refusal(&f);
                Err(r.text)
            }
        }
    }

    /// May this connection do `claim` on the event hub? Always, with the mode off.
    pub(crate) fn may(&self, c: &Conn, hub: &str, claim: Claim) -> bool {
        let Some(auth) = self.strict() else { return true };
        match &c.access {
            Access::Anonymous => false,
            Access::Rule { rights, entity } => rights.allows(claim) && entity.as_deref().is_none_or(|e| e.eq_ignore_ascii_case(hub)),
            Access::Principal(p) => {
                let res = Resource::event_hubs(&self.rules_ns(c)).at(format!("eventhubs/{hub}"));
                auth.registry().may(&p.oid, &Messaging::EventHubs.need(claim), &res)
            }
        }
    }

    /// Does the principal of this connection have any right at all in the namespace (on it, or on one of its event hubs)? What a coordinator
    /// lookup asks: a connection with no role is closed there. A connection string always has one.
    pub(crate) fn has_any_right(&self, c: &Conn) -> bool {
        let Some(auth) = self.strict() else { return true };
        match &c.access {
            Access::Anonymous => false,
            Access::Rule { .. } => true,
            Access::Principal(p) => {
                let rns = self.rules_ns(c);
                let any = |res: &Resource| {
                    [Claim::Send, Claim::Listen].iter().any(|cl| auth.registry().may(&p.oid, &Messaging::EventHubs.need(*cl), res))
                };
                if any(&Resource::event_hubs(&rns)) {
                    return true;
                }
                let hubs: Vec<String> = self
                    .shared
                    .state
                    .lock()
                    .namespaces
                    .get(&rns)
                    .map(|n| n.hubs.values().map(|h| h.name.clone()).collect())
                    .unwrap_or_default();
                hubs.iter().any(|h| any(&Resource::event_hubs(&rns).at(format!("eventhubs/{h}"))))
            }
        }
    }

    /// Log a refusal: one line, the way the other services do.
    pub(crate) fn log_refused(&self, c: &Conn, what: &str, hub: &str, claim: Claim) {
        let who = match &c.access {
            Access::Anonymous => "an anonymous connection".to_string(),
            Access::Rule { rights, .. } => format!("a connection string whose rule has {}", rights.names().join(", ")),
            Access::Principal(p) => format!("principal {}", p.oid),
        };
        tracing::info!("refused kafka {what} on {hub}: {who} lacks the '{}' claim", claim.word());
    }
}
