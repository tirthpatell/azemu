//! Strict authorisation of the administration API (`docs/strict-auth.md`): the credential of the `Authorization` header, then the rights it
//! gives for the call. Everything here is what Azure answered in November 2026, and every refusal is `401` (never 403), in the same XML.
//!
//! * A **SAS** token needs `Manage` for every call on an entity (`Manage,EntityRead claims required for this operation.` for a read, `Authorization
//!   failed for specified action: Manage,EntityWrite.` for a write); the namespace information is open to any rule. A rule of one entity is no
//!   rule at all for another entity or for a list (`InvalidSignature`).
//! * An **Entra token** needs the `actions` of a role, not its data actions: `Microsoft.ServiceBus/namespaces/queues/read` for a get or a list
//!   (so a Data Receiver, a Data Sender on that one queue, and any Reader may read), `.../write` and `.../delete` for the changes (which no
//!   data role has). That is how the answers for the four principals come out.

use azemu_core::auth::cbs;
use azemu_core::auth::sas::{self, SasFailure};
use azemu_core::auth::{Aud, Need, Principal, Resource, TokenFailure};

use super::error::{ApiError, ErrKind};
use super::Cx;
use crate::state::Shared;

const PREFIX: &str = "Microsoft.ServiceBus/namespaces";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Read,
    Write,
    Delete,
}

/// What a request is about: where the registry looks, which action it names, and the path a SAS token has to cover.
struct Op {
    act: Act,
    /// `queues/q`, `topics/t/subscriptions/s`; empty for the namespace (a list, the namespace information)
    res_path: String,
    /// `queues`, `topics`, `topics/subscriptions`, `topics/subscriptions/rules`
    kind: &'static str,
    /// The entity path, for the SAS audience and the rules that apply (empty: the namespace)
    entity: String,
    /// The namespace information: any rule, a principal that can read anything
    info: bool,
}

enum Who {
    Rule(sas::Rights),
    Principal(Principal),
}

fn unauthorized(text: impl Into<String>) -> ApiError {
    ApiError::new(ErrKind::Unauthorized(text.into()))
}

fn principal_failure(f: &TokenFailure) -> ApiError {
    unauthorized(match f {
        TokenFailure::Malformed => "MalformedToken: .".to_string(),
        TokenFailure::BadSignature => "InvalidSignature: Invalid authorization token signature.".to_string(),
        TokenFailure::WrongAudience(_) => "InvalidAudience: Invalid authorization token audience.".to_string(),
        // an Entra token could not be made to expire on Azure: this is the text of an expired SAS token
        TokenFailure::Expired(exp) => format!("ExpiredToken: The token is expired. Expiration time: '{}'.", expiry(*exp)),
        TokenFailure::NotYetValid(_) => "InvalidToken: The token is not valid yet.".to_string(),
    })
}

fn expiry(secs: i64) -> String {
    let r = cbs::sas_refusal(&SasFailure::Expired(secs)).text;
    r.split('\'').nth(1).unwrap_or("").to_string()
}

fn classify(shared: &Shared, cx: &Cx, segs: &[String]) -> Option<Op> {
    let method = cx.req.method.as_str();
    let act = match method {
        "GET" | "HEAD" => Act::Read,
        "DELETE" => Act::Delete,
        _ => Act::Write,
    };
    let is = |i: usize, w: &str| segs.get(i).is_some_and(|s| s.eq_ignore_ascii_case(w));
    let op = |kind: &'static str, res: String, entity: String| Op { act, res_path: res, kind, entity, info: false };
    match segs.len() {
        0 => None,
        1 if is(0, "$namespaceinfo") => {
            Some(Op { act: Act::Read, res_path: String::new(), kind: "queues", entity: String::new(), info: true })
        }
        // the service answers any other `$` name with its list of services
        1 if segs[0].starts_with('$') => None,
        1 => {
            let name = &segs[0];
            let is_topic = shared.state.lock().ns(&cx.account).topic(name).is_some();
            let (kind, dir) = if is_topic { ("topics", "topics") } else { ("queues", "queues") };
            Some(op(kind, format!("{dir}/{name}"), name.to_ascii_lowercase()))
        }
        2 if is(0, "$Resources") => {
            let kind = if is(1, "topics") { "topics" } else { "queues" };
            Some(op(kind, String::new(), String::new()))
        }
        2 if is(1, "subscriptions") => Some(op("topics/subscriptions", format!("topics/{}", segs[0]), segs[0].to_ascii_lowercase())),
        3 if is(1, "subscriptions") => Some(op(
            "topics/subscriptions",
            format!("topics/{}/subscriptions/{}", segs[0], segs[2]),
            format!("{}/subscriptions/{}", segs[0], segs[2]).to_ascii_lowercase(),
        )),
        4 | 5 if is(1, "subscriptions") && is(3, "rules") => {
            let sub = format!("topics/{}/subscriptions/{}", segs[0], segs[2]);
            let res = if segs.len() == 5 { format!("{sub}/rules/{}", segs[4]) } else { sub };
            Some(op("topics/subscriptions/rules", res, format!("{}/subscriptions/{}", segs[0], segs[2]).to_ascii_lowercase()))
        }
        _ => None,
    }
}

/// The credential, checked for the entity of the request.
fn who(shared: &Shared, cx: &Cx, header: &str, entity: &str) -> Result<Who, ApiError> {
    let auth = shared.auth();
    let now = cx.now / 1000;
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !rest.is_empty() {
        let auds = [
            Aud::exact("https://servicebus.azure.net"),
            Aud::exact("https://eventhubs.azure.net"),
            Aud::host(&cx.account.to_ascii_lowercase()),
        ];
        return auth.authenticate_token(rest, now, &auds).map(Who::Principal).map_err(|f| principal_failure(&f));
    }
    if scheme.eq_ignore_ascii_case("SharedAccessSignature") {
        // what cannot be read is a 500 on Azure, with nothing but the tracking text
        let Some(t) = sas::parse_token(header) else { return Err(ApiError::new(ErrKind::Bare500)) };
        let rns = shared.state.lock().resolve(&cx.account);
        let rules = shared.sas.candidates(&rns, &sas::chain_of(entity));
        // the host of the audience must be the namespace the request is for
        let ours = |h: &str| {
            let n = crate::amqp_conn::namespace_from_hostname(Some(h));
            n == cx.account.to_ascii_lowercase() || n == rns
        };
        return match sas::check(&t, &rules, entity, now, &ours) {
            Ok(rule) => Ok(Who::Rule(rule.rights)),
            // the administration API ends the expiry text with a full stop (`...Expiration time: '<time>'. TrackingId:`)
            Err(f @ SasFailure::Expired(_)) => Err(unauthorized(format!("{}.", strip_tracking(&cbs::sas_refusal(&f).text)))),
            Err(f @ (SasFailure::Malformed | SasFailure::WrongAudience)) => Err(unauthorized(strip_tracking(&cbs::sas_refusal(&f).text))),
            Err(SasFailure::UnknownKey | SasFailure::BadSignature) => {
                Err(unauthorized("InvalidSignature: The token has an invalid signature."))
            }
        };
    }
    Err(unauthorized("claim is empty or token is invalid."))
}

fn strip_tracking(t: &str) -> String {
    t.split(" TrackingId:").next().unwrap_or(t).to_string()
}

/// The refusal of a request that the credential is good for but whose rights are missing.
fn refusal(op: &Op, is_rule: bool) -> ApiError {
    let at = |t: &str| ApiError::new(ErrKind::UnauthorizedAt(t.to_string()));
    match (op.act, is_rule) {
        (Act::Read, true) => at("Manage,EntityRead claims required for this operation."),
        (Act::Read, false) => at("Authorization failed for specified action: Manage,EntityRead,Send,Listen."),
        (Act::Write, _) => at("Authorization failed for specified action: Manage,EntityWrite."),
        (Act::Delete, _) => at("Authorization failed for specified action: Manage,EntityDelete."),
    }
}

/// Check the request. Called with the mode on and an `Authorization` header present.
pub(super) fn check(shared: &Shared, cx: &Cx, segs: &[String], header: &str) -> Result<(), ApiError> {
    let auth = shared.auth();
    let op = classify(shared, cx, segs);
    let entity = op.as_ref().map(|o| o.entity.clone()).unwrap_or_default();
    let who = who(shared, cx, header, &entity)?;
    let Some(op) = op else { return Ok(()) };
    let ns = shared.state.lock().resolve(&cx.account);
    let what = format!("{} /{}", cx.req.method, segs.join("/"));
    let ok = match &who {
        Who::Rule(rights) => op.info || rights.manage,
        Who::Principal(p) => {
            let res = if op.res_path.is_empty() { Resource::service_bus(&ns) } else { Resource::service_bus(&ns).at(op.res_path.clone()) };
            let verb = match op.act {
                Act::Read => "read",
                Act::Write => "write",
                Act::Delete => "delete",
            };
            let need = if op.info {
                Need::any(vec![
                    Need::control(&format!("{PREFIX}/read")),
                    Need::control(&format!("{PREFIX}/queues/read")),
                    Need::control(&format!("{PREFIX}/topics/read")),
                ])
            } else {
                Need::control(&format!("{PREFIX}/{}/{verb}", op.kind))
            };
            auth.authorize(&what, p, &need, &res).is_ok()
        }
    };
    if ok {
        return Ok(());
    }
    if let Who::Rule(r) = &who {
        tracing::info!("refused {what}: the rule has {}, the call needs Manage", r.names().join(", "));
    }
    // a delete of what is not there: anybody with a right to read is told it is not there; the others are not told anything useful
    if op.act == Act::Delete {
        let reads = match &who {
            Who::Rule(_) => true,
            Who::Principal(p) => {
                let res = Resource::service_bus(&ns).at(op.res_path.clone());
                let need = Need::control(&format!("{PREFIX}/{}/read", op.kind));
                auth.registry().may(&p.oid, &need, &res)
            }
        };
        if reads {
            let missing = !entity_exists(shared, cx, segs);
            if missing {
                return Ok(());
            }
        } else {
            return Err(unauthorized("Generic: ."));
        }
    }
    Err(refusal(&op, matches!(who, Who::Rule(_))))
}

fn entity_exists(shared: &Shared, cx: &Cx, segs: &[String]) -> bool {
    let mut st = shared.state.lock();
    let ns = st.ns(&cx.account);
    match segs {
        [name] => ns.queue(name).is_some() || ns.topic(name).is_some(),
        [t, _, s] => ns.topic(t).map(|t| t.subs.contains_key(&crate::state::key(s))).unwrap_or(false),
        [t, _, s, _, r] => ns
            .topic(t)
            .and_then(|t| t.subs.get(&crate::state::key(s)))
            .map(|s| s.rules.iter().any(|x| x.name.eq_ignore_ascii_case(r)))
            .unwrap_or(false),
        _ => true,
    }
}
