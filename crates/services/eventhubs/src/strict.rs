//! Strict authorisation of the AMQP side (`docs/strict-auth.md`): what `$cbs` `put-token` checks, what a link attach needs, and what a
//! `$management` read needs of what the connection holds. None of it is looked at unless the mode is on.
//!
//! Azure's steps (found in November 2026): a bad credential is refused at `put-token` (`401`, `com.microsoft:auth-failed`); a good one is
//! accepted whatever its rights, and the missing claim shows at the link attach (answered with no source and target, then detached with
//! `amqp:unauthorized-access`). `$management` is different: its links attach, and the refusal is a `401` *inside* the answer to the `READ`
//! (`amqp:unauthorized-access`, `Attempted to perform an unauthorized operation.`), for a connection that may neither send nor receive.

use std::sync::Arc;

use azemu_amqp::api::{AttachRequest, LinkHandle, LinkId};
use azemu_amqp::message::{Body, Message};
use azemu_amqp::performatives::Role;
use azemu_amqp::terminus::TargetKind;
use azemu_amqp::Error as AmqpError;
use azemu_core::auth::cbs::{self, Messaging, PutToken, Refusal};
use azemu_core::auth::claims::ConnClaims;
use azemu_core::auth::sas::Claim;
use azemu_core::auth::Auth;

use crate::address::Address;
use crate::amqp_conn::EhConn;
use crate::errors::EhError;

fn unix_now() -> i64 {
    azemu_core::time::OffsetDateTime::now_utc().unix_timestamp()
}

fn token_text(m: &Message) -> String {
    match &m.body {
        Body::Value(v) => {
            v.as_str().map(str::to_string).or_else(|| v.as_binary().map(|b| String::from_utf8_lossy(b).into_owned())).unwrap_or_default()
        }
        Body::Data(parts) => String::from_utf8_lossy(&parts.concat()).into_owned(),
        _ => String::new(),
    }
}

pub(crate) struct Tracked {
    pub path: String,
}

impl EhConn {
    pub(crate) fn strict_auth(&self) -> Option<Auth> {
        let a = self.shared.auth();
        a.is_strict().then_some(a)
    }

    /// The namespace the rules and roles are looked up under (a config file that names one namespace gives the name `default` to it).
    fn rules_ns(&self) -> String {
        self.shared.state.lock().resolve(&self.ns)
    }

    fn conn_claims(&mut self) -> Arc<ConnClaims> {
        let id = self.conn.id();
        self.claims.get_or_insert_with(|| ConnClaims::for_conn(id)).clone()
    }

    /// `put-token` in strict mode: `None` when the mode is off (the caller keeps accepting every credential).
    pub(crate) fn strict_put_token(&mut self, req: &Message) -> Option<Result<(), Refusal>> {
        let auth = self.strict_auth()?;
        let name = req.app_property("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let token = token_text(req);
        let claims = self.conn_claims();
        let rns = self.rules_ns();
        let ns = self.ns.clone();
        Some(cbs::put_token(
            &auth,
            &self.shared.sas,
            Messaging::EventHubs,
            &claims,
            &PutToken {
                namespace: &self.ns,
                rules_ns: &rns,
                name: &name,
                token: &token,
                now: unix_now(),
                // the host of the audience must be this namespace, as the connection's own `open` names it
                host_is_ours: &|h| {
                    let n = crate::amqp_conn::namespace_from_hostname(Some(h));
                    n == ns || n == rns
                },
            },
        ))
    }

    /// The path the registry looks at for an event hub.
    fn registry_path(hub: &str) -> String {
        format!("eventhubs/{hub}")
    }

    fn strict_claim(&mut self, auth: &Auth, hub: &str, path: &str, shown: &str, claim: Claim) -> Result<(), EhError> {
        let claims = self.conn_claims();
        let rns = self.rules_ns();
        if cbs::has_claim(auth, &claims, Messaging::EventHubs, &rns, path, &Self::registry_path(hub), claim) {
            return Ok(());
        }
        tracing::info!(
            "refused {} link to {}: the '{}' claim is missing ({})",
            if claim == Claim::Send { "sender" } else { "receiver" },
            path,
            claim.word(),
            cbs::describe_holders(&claims, path)
        );
        Err(EhError::new(cbs::UNAUTHORIZED_ACCESS, 401, cbs::missing_claim_text(claim, &self.ns, shown)))
    }

    /// Before a link to an event hub or one of its partitions is attached.
    pub(crate) fn strict_attach(&mut self, req: &AttachRequest) -> Option<Result<Tracked, AmqpError>> {
        let auth = self.strict_auth()?;
        if let Some(TargetKind::Coordinator(_)) = &req.attach.target {
            return None;
        }
        let role = req.local_role();
        let raw = match role {
            Role::Receiver => req.attach.target.as_ref().and_then(|t| t.address()).unwrap_or_default().to_string(),
            Role::Sender => req.attach.source.as_ref().and_then(|s| s.address.clone()).unwrap_or_default(),
        };
        let addr = Address::parse(&raw)?;
        let (hub, path, shown, claim) = match (&addr, role) {
            (Address::Hub(h), Role::Receiver) => (h.clone(), h.to_ascii_lowercase(), h.clone(), Claim::Send),
            (Address::Partition { hub, pid }, Role::Receiver) => {
                (hub.clone(), format!("{hub}/partitions/{pid}").to_ascii_lowercase(), format!("{hub}/partitions/{pid}"), Claim::Send)
            }
            (Address::Consumer { hub, cg, pid }, Role::Sender) => {
                let p = format!("{hub}/consumergroups/{cg}/partitions/{pid}");
                // the resource in Azure's text is all lower case, the partition included (found in November 2026)
                (hub.clone(), p.to_ascii_lowercase(), p.to_ascii_lowercase(), Claim::Listen)
            }
            // the wrong direction, `$cbs` and `$management`: the attach itself answers
            _ => return None,
        };
        if let Err(e) = self.strict_claim(&auth, &hub, &path, &shown, claim) {
            return Some(Err(e.to_amqp()));
        }
        Some(Ok(Tracked { path }))
    }

    pub(crate) fn strict_track(&mut self, link: LinkId, handle: LinkHandle, t: Tracked) {
        let claims = self.conn_claims();
        let ns = self.ns.clone();
        let path = t.path.clone();
        claims.track_link(
            link,
            &t.path,
            Box::new(move || {
                // not seen on Azure (a token that expires while a link is open could not be waited for): the documented condition for a link whose
                // authorisation ends is `amqp:unauthorized-access`
                let text = format!(
                    "Unauthorized access. The credential for 'sb://{ns}.servicebus.windows.net/{path}' has expired and was not renewed. {}",
                    cbs::tracking()
                );
                handle.detach(Some(AmqpError::new(cbs::UNAUTHORIZED_ACCESS, text)));
            }),
        );
    }

    pub(crate) fn strict_forget(&mut self, link: LinkId) {
        if let Some(c) = &self.claims {
            c.forget_link(link);
        }
    }

    /// A `$management` read for an event hub: the connection must be able to send or to receive there. `Err`: the answer to give.
    pub(crate) fn strict_management(&mut self, hub: &str) -> Result<(), EhError> {
        let Some(auth) = self.strict_auth() else { return Ok(()) };
        let path = hub.to_ascii_lowercase();
        let claims = self.conn_claims();
        let rns = self.rules_ns();
        let may = |c: Claim| cbs::has_claim(&auth, &claims, Messaging::EventHubs, &rns, &path, &Self::registry_path(hub), c);
        if may(Claim::Send) || may(Claim::Listen) {
            return Ok(());
        }
        tracing::info!(
            "refused $management read of {path}: the connection can neither send nor receive there ({})",
            cbs::describe_holders(&claims, &path)
        );
        Err(EhError::new(cbs::UNAUTHORIZED_ACCESS, 401, cbs::MANAGEMENT_UNAUTHORIZED))
    }
}
