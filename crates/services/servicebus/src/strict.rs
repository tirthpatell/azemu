//! Strict authorisation of the AMQP side (`docs/strict-auth.md`): what `$cbs` `put-token` checks, and what a link attach and a `$management`
//! request need of what the connection holds. All of it is off, and none of it is looked at, unless the mode is on.
//!
//! The steps are Azure's (found in November 2026): a bad credential is refused at `put-token` (`401`, `com.microsoft:auth-failed`); a good one
//! is accepted whatever its rights, and the missing claim shows at the link attach, which is answered with no source and target and then
//! detached with `amqp:unauthorized-access`. The logic that Service Bus and Event Hubs share is in `azemu_core::auth::cbs`.

use std::sync::Arc;

use azemu_amqp::api::{AttachRequest, LinkId};
use azemu_amqp::message::{Body, Message};
use azemu_amqp::performatives::Role;
use azemu_amqp::terminus::TargetKind;
use azemu_amqp::Error as AmqpError;
use azemu_core::auth::cbs::{self, Messaging, PutToken, Refusal};
use azemu_core::auth::claims::ConnClaims;
use azemu_core::auth::sas::Claim;
use azemu_core::auth::Auth;

use crate::amqp_conn::{SbConn, TRANSFER_DESTINATION};
use crate::errors::SbError;
use crate::model::Address;

fn unix_now() -> i64 {
    azemu_core::time::OffsetDateTime::now_utc().unix_timestamp()
}

/// The text of a token in a `put-token` request: the body, as a string or as bytes.
fn token_text(m: &Message) -> String {
    match &m.body {
        Body::Value(v) => {
            v.as_str().map(str::to_string).or_else(|| v.as_binary().map(|b| String::from_utf8_lossy(b).into_owned())).unwrap_or_default()
        }
        Body::Data(parts) => String::from_utf8_lossy(&parts.concat()).into_owned(),
        _ => String::new(),
    }
}

/// The path of an address as the client wrote it, without scheme and host: what a refusal names as the resource.
fn written_path(raw: &str) -> String {
    let s = raw.trim();
    match s.find("://") {
        Some(i) => s[i + 3..].split_once('/').map(|(_, p)| p).unwrap_or("").trim_matches('/').to_string(),
        None => s.trim_matches('/').to_string(),
    }
}

/// The claim a `$management` operation needs. Not seen on Azure (the SDKs open a sender or receiver link before they ask, so the link
/// attach refuses first); these are what the operation does.
pub(crate) fn management_claim(op: &str) -> Claim {
    match op {
        "com.microsoft:schedule-message" | "com.microsoft:cancel-scheduled-message" => Claim::Send,
        "com.microsoft:add-rule" | "com.microsoft:remove-rule" | "com.microsoft:enumerate-rules" => Claim::Manage,
        _ => Claim::Listen,
    }
}

/// What a tracked link needs when its credential runs out.
pub(crate) struct Tracked {
    pub path: String,
}

impl SbConn {
    /// The handle, when the mode is on.
    pub(crate) fn strict_auth(&self) -> Option<Auth> {
        let a = self.shared.auth();
        a.is_strict().then_some(a)
    }

    /// The namespace the rules and roles are looked up under (a config file that names one namespace gives the name `default` to it).
    fn rules_ns(&self) -> String {
        self.shared.state.lock().resolve(&self.ns)
    }

    fn conn_claims(&mut self) -> Arc<ConnClaims> {
        let id = self._conn.id();
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
            Messaging::ServiceBus,
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

    /// The path the registry looks at for an address, `queues/q`, `topics/t`, `topics/t/subscriptions/s`. `send`: a name that is a topic is the topic.
    fn registry_path(&self, addr: &Address, send: bool) -> String {
        match addr {
            Address::Queue { name, .. } => {
                let is_topic = send && self.shared.state.lock().ns(&self.ns).topic(name).is_some();
                format!("{}/{name}", if is_topic { "topics" } else { "queues" })
            }
            Address::Topic { name } => format!("topics/{name}"),
            Address::Subscription { topic, name, .. } => format!("topics/{topic}/subscriptions/{name}"),
            Address::Management(p) => Address::parse(p).map(|a| self.registry_path(&a, send)).unwrap_or_default(),
            Address::Cbs => String::new(),
        }
    }

    /// Does the connection hold the claim for the entity at `raw`? `Err` is the refusal, with the text Azure gives.
    pub(crate) fn strict_claim(&mut self, auth: &Auth, raw: &str, addr: &Address, claim: Claim) -> Result<(), SbError> {
        let path = cbs::name_path(raw);
        let res_path = self.registry_path(addr, claim == Claim::Send);
        let claims = self.conn_claims();
        let rns = self.rules_ns();
        if cbs::has_claim(auth, &claims, Messaging::ServiceBus, &rns, &path, &res_path, claim) {
            return Ok(());
        }
        tracing::info!(
            "refused {} link to {}: the '{}' claim is missing ({})",
            if claim == Claim::Send { "sender" } else { "receiver" },
            path,
            claim.word(),
            cbs::describe_holders(&claims, &path)
        );
        Err(SbError::new(cbs::UNAUTHORIZED_ACCESS, 401, cbs::missing_claim_text(claim, &self.ns, &written_path(raw))))
    }

    /// Before an entity link is attached: `Some(refusal)` when the connection lacks the claim. `None` for the mode off and for the nodes
    /// that need no claim (`$cbs`, `$management`, the transaction coordinator).
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
        // an address that cannot be parsed is refused by the attach itself
        let addr = Address::parse(&raw)?;
        if matches!(addr, Address::Cbs | Address::Management(_)) {
            return None;
        }
        let claim = if role == Role::Receiver { Claim::Send } else { Claim::Listen };
        if let Err(e) = self.strict_claim(&auth, &raw, &addr, claim) {
            return Some(Err(e.to_amqp()));
        }
        // a send-via link also sends to the entity its messages are for
        if role == Role::Receiver {
            let dest =
                req.attach.properties.as_ref().and_then(|p| p.map_get(TRANSFER_DESTINATION)).and_then(|v| v.as_str()).map(str::to_string);
            if let Some(d) = dest {
                if let Some(da) = Address::parse(&d) {
                    if let Err(e) = self.strict_claim(&auth, &d, &da, Claim::Send) {
                        return Some(Err(e.to_amqp()));
                    }
                }
            }
        }
        Some(Ok(Tracked { path: cbs::name_path(&raw) }))
    }

    /// The link was attached: when the credentials that cover it run out, it is closed.
    pub(crate) fn strict_track(&mut self, link: LinkId, handle: azemu_amqp::api::LinkHandle, t: Tracked) {
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

    /// A `$management` request in strict mode: the claim the operation needs, for the entity of the node.
    pub(crate) fn strict_management(&mut self, node_path: &str, op: &str) -> Result<(), SbError> {
        let Some(auth) = self.strict_auth() else { return Ok(()) };
        let Some(addr) = Address::parse(node_path) else { return Ok(()) };
        self.strict_claim(&auth, node_path, &addr, management_claim(op))
    }
}
