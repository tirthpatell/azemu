//! The inspector of the identity service (`docs/inspector.md`): the signing key id and how many tokens it has signed. Nothing else is
//! kept (tokens and codes carry their own state), and nothing secret is shown: the key itself never leaves the service.

use std::sync::atomic::Ordering;

use azemu_core::inspect::{InspectError, InspectRequest, InspectResult, Node};
use serde_json::json;

use crate::IdentityService;

const ACCOUNT: &str = "signer";

impl IdentityService {
    pub(crate) fn inspect_node(&self, req: &InspectRequest) -> InspectResult {
        let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
        match segs.as_slice() {
            [] => Ok(Node::new(req.path(&[]), "service", "identity")
                .prop("accounts", 1)
                .with_items(vec![json!({"name": ACCOUNT, "path": req.path(&[ACCOUNT])})], None)),
            [ACCOUNT] => {
                // the key is read, never made: before it exists (it is generated in the background at start) the node says so
                let key = self.key.current.lock().clone();
                let mut n = Node::new(req.path(&[ACCOUNT]), "signer", "token signer").prop("algorithm", "RS256");
                n = match key {
                    Some(k) => n
                        .prop("keyReady", true)
                        .prop("keyId", &k.kid)
                        .prop("tokensSigned", k.signed.load(Ordering::Relaxed))
                        .prop("publicKey", k.jwk()),
                    None => n.prop("keyReady", false).prop("keyId", serde_json::Value::Null).prop("tokensSigned", 0),
                };
                Ok(n)
            }
            _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
        }
    }
}
