//! `POST .../storageAccounts/{name}/listAccountSas` and `.../listServiceSas`: a SAS token signed with an account key.
//! Source: the Storage resource provider REST API (`storageaccounts/listaccountsas`, `storageaccounts/listservicesas`;
//! the bodies are `AccountSasParameters` and `ServiceSasParameters`), and the string layouts of the data plane in
//! `azemu_core::storage::sig`, which verify what is signed here. The token is the query string without the `?`.
//!
//! The signed version is fixed (`SIGNED_VERSION`): the answer of Azure does not say which it uses.

use azemu_core::storage::sig::{account_sas_string_to_sign, hmac_sha256, service_sas_string_to_sign, SasFields, StorageService};
use base64::Engine;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Value};

use crate::error::{ArmError, ArmResult};
use crate::template::dotnet;

/// A version both the account and the service layouts of `azemu_core` know.
const SIGNED_VERSION: &str = "2022-11-02";

/// Everything but the unreserved characters is encoded, as the Storage SDKs write a SAS.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

fn text<'a>(body: &'a Value, name: &str) -> &'a str {
    body.get(name).and_then(Value::as_str).unwrap_or("")
}

/// `signedStart` and `signedExpiry` the way a SAS writes them: `yyyy-MM-ddTHH:mm:ssZ`.
fn time(body: &Value, name: &str, required: bool) -> ArmResult<String> {
    let t = text(body, name);
    if t.is_empty() {
        return if required {
            Err(ArmError::bad_request("InvalidInput", format!("The required property '{name}' is missing.")))
        } else {
            Ok(String::new())
        };
    }
    dotnet::parse(t).map(|d| dotnet::iso(&d)).ok_or_else(|| {
        ArmError::bad_request("InvalidInput", format!("'{t}' is not a valid value of '{name}': an ISO 8601 date and time was expected."))
    })
}

fn required<'a>(body: &'a Value, name: &str) -> ArmResult<&'a str> {
    let t = text(body, name);
    if t.is_empty() {
        Err(ArmError::bad_request("InvalidInput", format!("The required property '{name}' is missing.")))
    } else {
        Ok(t)
    }
}

fn key_to_sign(keys: &[String; 2], body: &Value) -> ArmResult<Vec<u8>> {
    let which = match text(body, "keyToSign").to_ascii_lowercase().as_str() {
        "" | "key1" => 0,
        "key2" => 1,
        other => return Err(ArmError::bad_request("InvalidInput", format!("keyToSign '{other}' is not valid; use key1 or key2."))),
    };
    base64::engine::general_purpose::STANDARD
        .decode(&keys[which])
        .map_err(|_| ArmError::new(500, "InternalServerError", "the account key cannot be read"))
}

fn query(fields: &[(&str, String)]) -> String {
    fields
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, QUERY)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `listAccountSas`: `{"accountSasToken": "sv=...&sig=..."}`.
pub fn account_sas(account: &str, keys: &[String; 2], body: &Value) -> ArmResult<Value> {
    let f = SasFields {
        sv: SIGNED_VERSION.into(),
        sp: required(body, "signedPermission")?.into(),
        ss: required(body, "signedServices")?.into(),
        srt: required(body, "signedResourceTypes")?.into(),
        st: time(body, "signedStart", false)?,
        se: time(body, "signedExpiry", true)?,
        sip: text(body, "signedIp").into(),
        spr: text(body, "signedProtocol").into(),
        ..SasFields::default()
    };
    let sig = hmac_sha256(&key_to_sign(keys, body)?, account_sas_string_to_sign(account, &f).as_bytes());
    let token = query(&[
        ("sv", f.sv.clone()),
        ("ss", f.ss.clone()),
        ("srt", f.srt.clone()),
        ("sp", f.sp.clone()),
        ("st", f.st.clone()),
        ("se", f.se.clone()),
        ("sip", f.sip.clone()),
        ("spr", f.spr.clone()),
        ("sig", base64::engine::general_purpose::STANDARD.encode(sig)),
    ]);
    Ok(json!({"accountSasToken": token}))
}

/// `listServiceSas` for Blob and Queue: `{"serviceSasToken": "sv=...&sig=..."}`. `canonicalizedResource` is
/// `/blob/{account}/{container}[/{blob}]` or `/queue/{account}/{queue}`.
pub fn service_sas(account: &str, keys: &[String; 2], body: &Value) -> ArmResult<Value> {
    let resource = required(body, "canonicalizedResource")?;
    let parts: Vec<&str> = resource.trim_start_matches('/').splitn(3, '/').collect();
    let (service, path) = match parts.as_slice() {
        [svc, acct, rest] if acct.eq_ignore_ascii_case(account) && !rest.is_empty() => (*svc, *rest),
        _ => {
            return Err(ArmError::bad_request(
                "InvalidInput",
                format!("canonicalizedResource '{resource}' must look like '/blob/{account}/container' or '/queue/{account}/queue' for this account."),
            ))
        }
    };
    let service = match service.to_ascii_lowercase().as_str() {
        "blob" => StorageService::Blob,
        "queue" => StorageService::Queue,
        other => {
            return Err(ArmError::not_implemented(format!("listServiceSas for the '{other}' service (only blob and queue are built)")))
        }
    };
    let signed_resource = text(body, "signedResource");
    if service == StorageService::Blob && signed_resource.is_empty() {
        return Err(ArmError::bad_request(
            "InvalidInput",
            "The required property 'signedResource' is missing (b, c, bs or bv for a blob SAS).",
        ));
    }
    let f = SasFields {
        sv: SIGNED_VERSION.into(),
        sp: required(body, "signedPermission")?.into(),
        sr: signed_resource.into(),
        st: time(body, "signedStart", false)?,
        se: time(body, "signedExpiry", true)?,
        sip: text(body, "signedIp").into(),
        spr: text(body, "signedProtocol").into(),
        si: text(body, "signedIdentifier").into(),
        rscc: text(body, "cacheControl").into(),
        rscd: text(body, "contentDisposition").into(),
        rsce: text(body, "contentEncoding").into(),
        rscl: text(body, "contentLanguage").into(),
        rsct: text(body, "contentType").into(),
        ..SasFields::default()
    };
    let snapshot = text(body, "signedSnapshotTime");
    let sig = hmac_sha256(&key_to_sign(keys, body)?, service_sas_string_to_sign(service, account, &f, path, snapshot, None).as_bytes());
    let token = query(&[
        ("sv", f.sv.clone()),
        ("sr", f.sr.clone()),
        ("sp", f.sp.clone()),
        ("st", f.st.clone()),
        ("se", f.se.clone()),
        ("sip", f.sip.clone()),
        ("spr", f.spr.clone()),
        ("si", f.si.clone()),
        ("rscc", f.rscc.clone()),
        ("rscd", f.rscd.clone()),
        ("rsce", f.rsce.clone()),
        ("rscl", f.rscl.clone()),
        ("rsct", f.rsct.clone()),
        ("sig", base64::engine::general_purpose::STANDARD.encode(sig)),
    ]);
    Ok(json!({"serviceSasToken": token}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> [String; 2] {
        let b = base64::engine::general_purpose::STANDARD;
        [b.encode([1u8; 32]), b.encode([2u8; 32])]
    }

    #[test]
    fn account_token_has_the_fields_and_a_signature() {
        let body =
            json!({"signedServices": "b", "signedResourceTypes": "sco", "signedPermission": "rl", "signedExpiry": "2030-08-20T11:00:00Z"});
        let t = account_sas("acct", &keys(), &body).unwrap()["accountSasToken"].as_str().unwrap().to_string();
        assert!(t.starts_with("sv=2022-11-02&ss=b&srt=sco&sp=rl&se=2030-08-20T11%3A00%3A00Z&sig="), "{t}");
        // the other key signs differently
        let t2 = account_sas("acct", &keys(), &json!({"signedServices": "b", "signedResourceTypes": "sco", "signedPermission": "rl", "signedExpiry": "2030-08-20T11:00:00Z", "keyToSign": "key2"})).unwrap();
        assert_ne!(t2["accountSasToken"].as_str().unwrap(), t);
        assert!(account_sas("acct", &keys(), &json!({"signedServices": "b"})).is_err());
        assert!(account_sas(
            "acct",
            &keys(),
            &json!({"signedServices": "b", "signedResourceTypes": "s", "signedPermission": "r", "signedExpiry": "soon"})
        )
        .is_err());
    }

    #[test]
    fn service_token_checks_the_resource() {
        let ok = json!({"canonicalizedResource": "/blob/acct/c/b.txt", "signedResource": "b", "signedPermission": "r", "signedExpiry": "2030-01-01T00:00:00Z"});
        let t = service_sas("acct", &keys(), &ok).unwrap()["serviceSasToken"].as_str().unwrap().to_string();
        assert!(t.starts_with("sv=2022-11-02&sr=b&sp=r&se=2030-01-01T00%3A00%3A00Z&sig="), "{t}");
        let other = json!({"canonicalizedResource": "/blob/other/c", "signedResource": "c", "signedPermission": "r", "signedExpiry": "2030-01-01T00:00:00Z"});
        assert!(service_sas("acct", &keys(), &other).is_err());
        let table = json!({"canonicalizedResource": "/table/acct/t", "signedPermission": "r", "signedExpiry": "2030-01-01T00:00:00Z"});
        assert_eq!(service_sas("acct", &keys(), &table).unwrap_err().status, 501);
        let q = json!({"canonicalizedResource": "/queue/acct/q", "signedPermission": "r", "signedExpiry": "2030-01-01T00:00:00Z"});
        assert!(service_sas("acct", &keys(), &q).unwrap()["serviceSasToken"].as_str().unwrap().contains("sig="));
    }
}
