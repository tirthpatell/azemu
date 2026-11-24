//! The inspector of this service (`docs/inspector.md`): a read-only look at the vaults.
//!
//! Rules this file keeps:
//! - Nothing is created and nothing is swept. A vault that does not exist is a 404, and `State::vault` (which creates the vault and
//!   drops purged items) is never called. Whether a deleted item is past its purge date is worked out here.
//! - A secret value is returned only for one secret (or one version of it) asked for with `?reveal=1`, and never for the secret behind a
//!   certificate (it holds the private key). Private key material and the pending key of a certificate request are never read out:
//!   a key is shown as its public JWK only.
//!
//! Tree (below `/keyvault`): `{vault}`, then `secrets`, `keys`, `certificates` (each with `{name}`, `{name}/versions`,
//! `{name}/versions/{version}`), the same three as `deletedsecrets`, `deletedkeys`, `deletedcertificates` (`{name}` only) and `issuers`.

use std::borrow::Cow;
use std::ops::Bound::{Excluded, Unbounded};

use azemu_core::inspect::{unix_time_value, InspectError, InspectRequest, InspectResult, Node};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::certificates::{stub_version, CertExtra, CertVersion};
use crate::common::{Attrs, Tags, RECOVERABLE_DAYS, RECOVERY_LEVEL};
use crate::keys::{KeyExtra, KeyVersion};
use crate::secrets::SecretVersion;
use crate::state::{norm, Collection, Item, Vault, Versioned};
use crate::KeyVaultService;

/// What the three kinds of object have in common for the inspector.
trait Inspected: Versioned + Clone + Sized {
    type Extra;
    /// `secret`, `key`, `certificate`; the group is `{SINGULAR}s`, the deleted group `deleted{SINGULAR}s`.
    const SINGULAR: &'static str;
    /// Does the node of one of these offer `?reveal=1`?
    const REVEALABLE: bool = false;
    fn attrs(&self) -> &Attrs;
    fn tags(&self) -> &Tags;
    /// Fields of one version that go into rows and detail alike.
    fn fields(&self) -> Map<String, Value>;
    /// More fields of a version's own node (the public key, the certificate).
    fn detail(&self, _reveal: bool) -> Map<String, Value> {
        Map::new()
    }
    /// Fields of the object, not of a version (rotation policy, certificate policy).
    fn item_detail(_item: &Item<Self, Self::Extra>) -> Map<String, Value> {
        Map::new()
    }
    /// A certificate whose request never produced a certificate has no version: its bundle stands in for one.
    fn stub(_item: &Item<Self, Self::Extra>) -> Option<Self> {
        None
    }
}

impl Inspected for SecretVersion {
    type Extra = ();
    const SINGULAR: &'static str = "secret";
    const REVEALABLE: bool = true;
    fn attrs(&self) -> &Attrs {
        &self.attrs
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("contentType".into(), json!(self.content_type));
        m.insert("managed".into(), json!(self.managed));
        m.insert("kid".into(), json!(self.kid));
        m
    }
    fn detail(&self, reveal: bool) -> Map<String, Value> {
        let mut m = Map::new();
        if reveal {
            if self.managed {
                // the value of the secret behind a certificate is a PFX or PEM with the private key
                m.insert(
                    "valueWithheld".into(),
                    json!("This secret belongs to a certificate; its value holds the private key and is never shown."),
                );
            } else {
                m.insert("value".into(), json!(self.value));
            }
        }
        m
    }
}

impl Inspected for KeyVersion {
    type Extra = KeyExtra;
    const SINGULAR: &'static str = "key";
    fn attrs(&self) -> &Attrs {
        &self.attrs
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kty".into(), json!(self.kty));
        m.insert("keyOps".into(), json!(self.key_ops));
        m.insert("keySize".into(), json!(self.key_size));
        m.insert("exportable".into(), json!(self.exportable));
        m.insert("managed".into(), json!(self.managed));
        m
    }
    fn detail(&self, _reveal: bool) -> Map<String, Value> {
        let mut m = Map::new();
        // the public JWK only: kty, key_ops, n, e, crv, x, y
        let kid = format!("/keys/{}", self.version);
        m.insert("jwk".into(), crate::jwk::public_jwk(&kid, &self.kty, &self.key_ops, &self.material));
        m.insert("releasePolicy".into(), json!(self.release_policy));
        m
    }
    fn item_detail(item: &Item<Self, KeyExtra>) -> Map<String, Value> {
        let mut m = Map::new();
        if let Some(p) = &item.extra.policy {
            m.insert("rotationPolicy".into(), p.to_json("", &item.name));
        }
        m
    }
}

impl Inspected for CertVersion {
    type Extra = CertExtra;
    const SINGULAR: &'static str = "certificate";
    fn attrs(&self) -> &Attrs {
        &self.attrs
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    fn fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("thumbprint".into(), if self.x5t.is_empty() { Value::Null } else { json!(self.x5t) });
        let parsed = crate::cert_x509::parse_cert(&self.der).ok();
        m.insert("subject".into(), json!(parsed.as_ref().map(|p| p.subject.clone())));
        m
    }
    fn detail(&self, _reveal: bool) -> Map<String, Value> {
        let mut m = Map::new();
        // the certificate itself (public); the private key is never read
        m.insert("cer".into(), if self.der.is_empty() { Value::Null } else { json!(STANDARD.encode(&self.der)) });
        if let Ok(p) = crate::cert_x509::parse_cert(&self.der) {
            m.insert("notBeforeCertificate".into(), unix_time_value(p.not_before));
            m.insert("notAfterCertificate".into(), unix_time_value(p.not_after));
            m.insert("subjectAlternativeNames".into(), p.sans);
            m.insert("ekus".into(), json!(p.ekus));
            m.insert("keyUsage".into(), json!(p.key_usage));
            m.insert("isCa".into(), json!(p.is_ca));
            m.insert("keyType".into(), json!(p.kty));
            m.insert("keySize".into(), json!(p.key_size));
            m.insert("curve".into(), json!(p.curve.map(|c| c.name())));
        }
        m
    }
    fn item_detail(item: &Item<Self, CertExtra>) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("policy".into(), json!(item.extra.policy));
        if let Some(op) = &item.extra.operation {
            // the signing request is public; `pending_key` is not read
            m.insert(
                "pendingOperation".into(),
                json!({
                    "issuer": op.issuer,
                    "status": op.status,
                    "requestId": op.request_id,
                    "cancellationRequested": op.cancellation_requested,
                    "csr": STANDARD.encode(&op.csr),
                }),
            );
        }
        m
    }
    fn stub(item: &Item<Self, CertExtra>) -> Option<Self> {
        Some(stub_version(item))
    }
}

fn live<V, X>(item: &Item<V, X>, deleted: bool, now: i64) -> bool {
    match item.deleted {
        None => !deleted,
        // the service drops an item past its purge date the next time it is used; looking does not
        Some(d) => deleted && d.scheduled_purge_date > now,
    }
}

fn count<V, X>(c: &Collection<V, X>, deleted: bool, now: i64) -> u64 {
    c.items.values().filter(|i| live(i, deleted, now)).count() as u64
}

fn secs(t: Option<i64>) -> Value {
    t.map_or(Value::Null, unix_time_value)
}

fn latest<V: Inspected>(item: &Item<V, V::Extra>) -> Option<Cow<'_, V>> {
    item.latest().map(Cow::Borrowed).or_else(|| V::stub(item).map(Cow::Owned))
}

fn version_count<V: Inspected>(item: &Item<V, V::Extra>) -> usize {
    if item.versions.is_empty() && V::stub(item).is_some() {
        1
    } else {
        item.versions.len()
    }
}

/// The version `n` places from the newest.
fn newest_first<V: Inspected>(item: &Item<V, V::Extra>, n: usize) -> Option<Cow<'_, V>> {
    if item.versions.is_empty() {
        return if n == 0 { V::stub(item).map(Cow::Owned) } else { None };
    }
    item.versions.len().checked_sub(n + 1).map(|i| Cow::Borrowed(&item.versions[i]))
}

/// What one version looks like in a row or in a node.
fn version_map<V: Inspected>(v: &V) -> Map<String, Value> {
    let a = v.attrs();
    let mut m = Map::new();
    m.insert("version".into(), json!(v.version()));
    m.insert("enabled".into(), json!(a.enabled));
    m.insert("created".into(), unix_time_value(a.created));
    m.insert("updated".into(), unix_time_value(a.updated));
    m.insert("expires".into(), secs(a.exp));
    m.insert("notBefore".into(), secs(a.nbf));
    m.insert("recoveryLevel".into(), json!(RECOVERY_LEVEL));
    m.insert("recoverableDays".into(), json!(RECOVERABLE_DAYS));
    m.insert("tags".into(), json!(v.tags()));
    m.extend(v.fields());
    m
}

fn deleted_fields(item: &Item<impl Sized, impl Sized>, m: &mut Map<String, Value>) {
    if let Some(d) = item.deleted {
        m.insert("deletedDate".into(), unix_time_value(d.deleted_date));
        m.insert("scheduledPurgeDate".into(), unix_time_value(d.scheduled_purge_date));
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl KeyVaultService {
    pub(crate) fn inspect_node(&self, req: &InspectRequest) -> InspectResult {
        let st = self.state.lock();
        let now = now();
        let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
        let Some(&vault_name) = segs.first() else {
            return service_node(req, &st.vaults, now);
        };
        let Some(vault) = st.vaults.get(&norm(vault_name)) else {
            return Err(InspectError::not_found(format!("There is no vault '{vault_name}'.")));
        };
        match segs.as_slice() {
            [_] => Ok(vault_node(req, vault, now)),
            [_, "secrets"] => Ok(list(req, &vault.secrets, false, now)),
            [_, "keys"] => Ok(list(req, &vault.keys, false, now)),
            [_, "certificates"] => Ok(list(req, &vault.certs, false, now)),
            [_, "deletedsecrets"] => Ok(list(req, &vault.secrets, true, now)),
            [_, "deletedkeys"] => Ok(list(req, &vault.keys, true, now)),
            [_, "deletedcertificates"] => Ok(list(req, &vault.certs, true, now)),
            [_, "issuers"] => Ok(issuers(req, vault)),
            [_, "secrets", name, rest @ ..] => object(req, &vault.secrets, name, rest, false, now),
            [_, "keys", name, rest @ ..] => object(req, &vault.keys, name, rest, false, now),
            [_, "certificates", name, rest @ ..] => object(req, &vault.certs, name, rest, false, now),
            [_, "deletedsecrets", name, rest @ ..] => object(req, &vault.secrets, name, rest, true, now),
            [_, "deletedkeys", name, rest @ ..] => object(req, &vault.keys, name, rest, true, now),
            [_, "deletedcertificates", name, rest @ ..] => object(req, &vault.certs, name, rest, true, now),
            _ => Err(InspectError::not_found(format!("There is no node '{}'.", req.own_path()))),
        }
    }
}

fn service_node(req: &InspectRequest, vaults: &std::collections::HashMap<String, Vault>, now: i64) -> InspectResult {
    let mut names: Vec<&String> = vaults.keys().filter(|k| req.marker().is_none_or(|m| k.as_str() > m)).collect();
    names.sort();
    let (rows, next) = req.page_keyed(names.into_iter().map(|k| (k.as_str(), k)));
    let items = rows
        .into_iter()
        .map(|k| {
            let v = &vaults[k];
            json!({
                "name": k,
                "path": req.path(&[k]),
                "secrets": count(&v.secrets, false, now),
                "keys": count(&v.keys, false, now),
                "certificates": count(&v.certs, false, now),
            })
        })
        .collect();
    Ok(Node::new(req.path(&[]), "service", "keyvault").prop("accounts", vaults.len()).with_items(items, next))
}

fn vault_node(req: &InspectRequest, v: &Vault, now: i64) -> Node {
    let vn = req.segments[0].as_str();
    let mut n = Node::new(req.own_path(), "vault", vn)
        .prop("recoveryLevel", RECOVERY_LEVEL)
        .prop("recoverableDays", RECOVERABLE_DAYS)
        .prop("issuers", v.issuers.len())
        .prop("contacts", v.contacts.as_ref().map_or(0, Vec::len));
    for (group, kind, c) in [
        ("secrets", "secrets", count(&v.secrets, false, now)),
        ("keys", "keys", count(&v.keys, false, now)),
        ("certificates", "certificates", count(&v.certs, false, now)),
        ("deletedsecrets", "deletedsecrets", count(&v.secrets, true, now)),
        ("deletedkeys", "deletedkeys", count(&v.keys, true, now)),
        ("deletedcertificates", "deletedcertificates", count(&v.certs, true, now)),
    ] {
        n = n.prop(group, c).child(group, kind, Some(c), req.path(&[vn, group]));
    }
    n.child("issuers", "issuers", Some(v.issuers.len() as u64), req.path(&[vn, "issuers"]))
}

/// The row of a name in a list: the newest version's fields and how many versions there are.
fn item_row<V: Inspected>(req: &InspectRequest, group: &str, item: &Item<V, V::Extra>) -> Value {
    let vn = req.segments[0].as_str();
    let mut m = Map::new();
    m.insert("name".into(), json!(item.name));
    m.insert("path".into(), json!(req.path(&[vn, group, &item.name])));
    m.insert("versions".into(), json!(version_count(item)));
    if let Some(v) = latest(item) {
        m.extend(version_map(v.as_ref()));
    }
    deleted_fields(item, &mut m);
    Value::Object(m)
}

fn list<V: Inspected>(req: &InspectRequest, coll: &Collection<V, V::Extra>, deleted: bool, now: i64) -> Node {
    let group = format!("{}{}s", if deleted { "deleted" } else { "" }, V::SINGULAR);
    let start = req.marker().map_or(Unbounded, |m| Excluded(m.to_string()));
    let rows = coll.items.range((start, Unbounded)).filter(|(_, i)| live(i, deleted, now)).map(|(k, i)| (k.as_str(), i));
    let (found, next) = req.page_keyed(rows);
    let items = found.into_iter().map(|i| item_row(req, &group, i)).collect();
    Node::new(req.own_path(), &group, &group).prop("count", count(coll, deleted, now)).with_items(items, next)
}

fn object<V: Inspected>(
    req: &InspectRequest,
    coll: &Collection<V, V::Extra>,
    name: &str,
    rest: &[&str],
    deleted: bool,
    now: i64,
) -> InspectResult {
    let missing = || InspectError::not_found(format!("There is no node '{}'.", req.own_path()));
    let item = coll.items.get(&norm(name)).filter(|i| live(i, deleted, now)).ok_or_else(missing)?;
    let vn = req.segments[0].as_str();
    let group = format!("{}{}s", if deleted { "deleted" } else { "" }, V::SINGULAR);
    let reveal = req.flag("reveal") && !deleted;
    match rest {
        [] => {
            let kind = format!("{}{}", if deleted { "deleted" } else { "" }, V::SINGULAR);
            let mut n = Node::new(req.own_path(), &kind, name).prop("name", &item.name).prop("versions", version_count(item));
            if let Some(v) = latest(item) {
                n.properties.extend(version_map(v.as_ref()));
                n.properties.extend(v.detail(reveal));
            }
            n.properties.extend(V::item_detail(item));
            deleted_fields(item, &mut n.properties);
            if deleted {
                // a deleted object has no nodes below it: its versions are rows
                let (range, next) = req.page_range(version_count(item));
                let rows = range.filter_map(|i| newest_first(item, i)).map(|v| Value::Object(version_map(v.as_ref()))).collect();
                return Ok(n.with_items(rows, next));
            }
            if V::REVEALABLE {
                n = n.view("Reveal value", "reveal=1");
            }
            Ok(n.child("versions", "versions", Some(version_count(item) as u64), req.path(&[vn, &group, name, "versions"])))
        }
        ["versions"] if !deleted => {
            let (range, next) = req.page_range(version_count(item));
            let rows = range
                .filter_map(|i| newest_first(item, i))
                .map(|v| {
                    let mut m = version_map(v.as_ref());
                    m.insert("path".into(), json!(req.path(&[vn, &group, name, "versions", v.version()])));
                    Value::Object(m)
                })
                .collect();
            Ok(Node::new(req.own_path(), "versions", "versions").prop("count", version_count(item)).with_items(rows, next))
        }
        ["versions", version] if !deleted => {
            let v = if item.versions.is_empty() {
                V::stub(item).filter(|s| s.version() == *version).map(Cow::Owned)
            } else {
                item.find(version).map(Cow::Borrowed)
            }
            .ok_or_else(missing)?;
            let mut n = Node::new(req.own_path(), &format!("{}version", V::SINGULAR), *version).prop("name", &item.name);
            n.properties.extend(version_map(v.as_ref()));
            n.properties.extend(v.detail(reveal));
            if V::REVEALABLE {
                n = n.view("Reveal value", "reveal=1");
            }
            Ok(n)
        }
        _ => Err(missing()),
    }
}

fn issuers(req: &InspectRequest, v: &Vault) -> Node {
    let start = req.marker().map_or(Unbounded, |m| Excluded(m.to_string()));
    let (found, next) = req.page_keyed(v.issuers.range((start, Unbounded)).map(|(k, i)| (k.as_str(), i)));
    // the issuer's account id and password are not shown
    let items = found
        .into_iter()
        .map(|i| {
            json!({"name": i.name, "provider": i.provider, "enabled": i.enabled, "created": unix_time_value(i.created), "updated": unix_time_value(i.updated)})
        })
        .collect();
    Node::new(req.own_path(), "issuers", "issuers").prop("count", v.issuers.len()).with_items(items, next)
}
