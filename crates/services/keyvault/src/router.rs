//! Maps method + path to an operation.

use azemu_core::{Request, Response};

use crate::errors::{not_found_route, not_implemented};
use crate::keys_crypto::Op;
use crate::KeyVaultService;

pub async fn route(svc: &KeyVaultService, req: &Request) -> Response {
    let segments = req.segments();
    let s: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    let method = req.method.as_str();
    match s.first().copied() {
        Some("secrets") => secrets(svc, req, method, &s),
        Some("deletedsecrets") => deleted_secrets(svc, req, method, &s),
        Some("keys") => keys(svc, req, method, &s).await,
        Some("deletedkeys") => deleted_keys(svc, req, method, &s),
        Some("rng") if method == "POST" && s.len() == 1 => svc.random_bytes(req),
        Some("certificates") => certificates(svc, req, method, &s).await,
        Some("deletedcertificates") => deleted_certificates(svc, req, method, &s),
        Some("storage") | Some("deletedstorage") => not_implemented(
            &format!("{method} {} (managed storage accounts)", req.decoded_path()),
            &format!("KeyVault_ManagedStorage ({method})"),
        ),
        _ => not_found_route(method, &req.decoded_path()),
    }
}

/// The parts of a path below `/keys/{name}`, `/certificates/{name}` and so on that are fixed words; every other part is a
/// name or a version and is shown as `{}`.
const WORDS: &[&str] = &[
    "create",
    "import",
    "backup",
    "restore",
    "recover",
    "rotate",
    "rotationpolicy",
    "versions",
    "pending",
    "merge",
    "policy",
    "contacts",
    "issuers",
    "sign",
    "verify",
    "encrypt",
    "decrypt",
    "wrapkey",
    "unwrapkey",
    "wrap",
    "unwrap",
    "release",
    "getrandombytes",
];

/// A request no route matches: 501, named `KeyVault_<area> (<METHOD> /<area>/…)` with the names left out.
fn unsupported(req: &Request) -> Response {
    let segments = req.segments();
    let area = segments.first().map(String::as_str).unwrap_or("");
    let rest: Vec<&str> = segments.iter().skip(1).map(|s| if WORDS.contains(&s.as_str()) { s.as_str() } else { "{}" }).collect();
    let shape = format!("{} /{area}/{}", req.method, rest.join("/"));
    not_implemented(&format!("{} {}", req.method, req.decoded_path()), &format!("KeyVault_Unknown ({shape})"))
}

fn secrets(svc: &KeyVaultService, req: &Request, method: &str, s: &[&str]) -> Response {
    match (method, s) {
        ("GET", ["secrets"]) => svc.list_secrets(req),
        ("POST", ["secrets", "restore"]) => svc.restore_secret(req),
        ("PUT", ["secrets", name]) => svc.set_secret(req, name),
        ("DELETE", ["secrets", name]) => svc.delete_secret(req, name),
        ("GET", ["secrets", name, "versions"]) => svc.list_secret_versions(req, name),
        ("POST", ["secrets", name, "backup"]) => svc.backup_secret(req, name),
        ("GET", ["secrets", name]) => svc.get_secret(req, name, None),
        ("GET", ["secrets", name, version]) => svc.get_secret(req, name, Some(version)),
        ("PATCH", ["secrets", name]) => svc.update_secret(req, name, None),
        ("PATCH", ["secrets", name, version]) => svc.update_secret(req, name, Some(version)),
        _ => unsupported(req),
    }
}

fn deleted_secrets(svc: &KeyVaultService, req: &Request, method: &str, s: &[&str]) -> Response {
    match (method, s) {
        ("GET", ["deletedsecrets"]) => svc.list_deleted_secrets(req),
        ("GET", ["deletedsecrets", name]) => svc.get_deleted_secret(req, name),
        ("DELETE", ["deletedsecrets", name]) => svc.purge_secret(req, name),
        ("POST", ["deletedsecrets", name, "recover"]) => svc.recover_secret(req, name),
        _ => unsupported(req),
    }
}

async fn keys(svc: &KeyVaultService, req: &Request, method: &str, s: &[&str]) -> Response {
    match (method, s) {
        ("GET", ["keys"]) => svc.list_keys(req),
        ("POST", ["keys", "restore"]) => svc.restore_key(req).await,
        ("PUT", ["keys", name]) => svc.import_key(req, name).await,
        ("DELETE", ["keys", name]) => svc.delete_key(req, name),
        ("POST", ["keys", name, "create"]) => svc.create_key(req, name).await,
        ("POST", ["keys", name, "backup"]) => svc.backup_key(req, name),
        ("POST", ["keys", name, "rotate"]) => svc.rotate_key(req, name).await,
        ("GET", ["keys", name, "rotationpolicy"]) => svc.get_rotation_policy(req, name),
        ("PUT", ["keys", name, "rotationpolicy"]) => svc.update_rotation_policy(req, name),
        ("GET", ["keys", name, "versions"]) => svc.list_key_versions(req, name),
        ("GET", ["keys", name]) => svc.get_key(req, name, None),
        ("PATCH", ["keys", name]) => svc.update_key(req, name, None),
        ("GET", ["keys", name, version]) => svc.get_key(req, name, Some(version)),
        ("PATCH", ["keys", name, version]) => svc.update_key(req, name, Some(version)),
        // Crypto operations; a missing version means "latest".
        ("POST", ["keys", name, op]) => match Op::parse(op) {
            Some(op) => svc.key_operation(req, name, None, op).await,
            None => unsupported(req),
        },
        ("POST", ["keys", name, version, op]) => match Op::parse(op) {
            Some(op) => svc.key_operation(req, name, Some(version), op).await,
            None => unsupported(req),
        },
        _ => unsupported(req),
    }
}

fn deleted_keys(svc: &KeyVaultService, req: &Request, method: &str, s: &[&str]) -> Response {
    match (method, s) {
        ("GET", ["deletedkeys"]) => svc.list_deleted_keys(req),
        ("GET", ["deletedkeys", name]) => svc.get_deleted_key(req, name),
        ("DELETE", ["deletedkeys", name]) => svc.purge_key(req, name),
        ("POST", ["deletedkeys", name, "recover"]) => svc.recover_key(req, name),
        _ => unsupported(req),
    }
}

async fn certificates(svc: &KeyVaultService, req: &Request, method: &str, s: &[&str]) -> Response {
    match (method, s) {
        ("GET", ["certificates"]) => svc.list_certificates(req),
        ("GET", ["certificates", "contacts"]) => svc.get_contacts(req),
        ("PUT", ["certificates", "contacts"]) => svc.set_contacts(req),
        ("DELETE", ["certificates", "contacts"]) => svc.delete_contacts(req),
        ("GET", ["certificates", "issuers"]) => svc.list_issuers(req),
        ("GET", ["certificates", "issuers", name]) => svc.get_issuer(req, name),
        ("PUT", ["certificates", "issuers", name]) => svc.set_issuer(req, name),
        ("PATCH", ["certificates", "issuers", name]) => svc.update_issuer(req, name),
        ("DELETE", ["certificates", "issuers", name]) => svc.delete_issuer(req, name),
        ("POST", ["certificates", "restore"]) => svc.restore_certificate(req).await,
        ("DELETE", ["certificates", name]) => svc.delete_certificate(req, name),
        ("POST", ["certificates", name, "create"]) => svc.create_certificate(req, name).await,
        ("POST", ["certificates", name, "import"]) => svc.import_certificate(req, name).await,
        ("POST", ["certificates", name, "backup"]) => svc.backup_certificate(req, name),
        ("GET", ["certificates", name, "pending"]) => svc.get_certificate_operation(req, name),
        ("PATCH", ["certificates", name, "pending"]) => svc.update_certificate_operation(req, name),
        ("DELETE", ["certificates", name, "pending"]) => svc.delete_certificate_operation(req, name),
        ("POST", ["certificates", name, "pending", "merge"]) => svc.merge_certificate(req, name).await,
        ("GET", ["certificates", name, "policy"]) => svc.get_certificate_policy(req, name),
        ("PATCH", ["certificates", name, "policy"]) => svc.update_certificate_policy(req, name),
        ("GET", ["certificates", name, "versions"]) => svc.list_certificate_versions(req, name),
        ("GET", ["certificates", name]) => svc.get_certificate(req, name, None),
        ("PATCH", ["certificates", name]) => svc.update_certificate(req, name, None),
        ("GET", ["certificates", name, version]) => svc.get_certificate(req, name, Some(version)),
        ("PATCH", ["certificates", name, version]) => svc.update_certificate(req, name, Some(version)),
        _ => unsupported(req),
    }
}

fn deleted_certificates(svc: &KeyVaultService, req: &Request, method: &str, s: &[&str]) -> Response {
    match (method, s) {
        ("GET", ["deletedcertificates"]) => svc.list_deleted_certificates(req),
        ("GET", ["deletedcertificates", name]) => svc.get_deleted_certificate(req, name),
        ("DELETE", ["deletedcertificates", name]) => svc.purge_certificate(req, name),
        ("POST", ["deletedcertificates", name, "recover"]) => svc.recover_certificate(req, name),
        _ => unsupported(req),
    }
}
