//! Per-request context handed to providers.

use azemu_core::time::{iso8601_7, OffsetDateTime};
use azemu_core::Request;

use crate::Inner;

pub struct Cx<'a> {
    pub req: &'a Request,
    pub inner: &'a Inner,
    pub now: OffsetDateTime,
    /// Set when the request touched a resource of a type nothing emulates (stored as plain JSON): the answer then carries
    /// `x-azemu-stored-only: true`. One flag for the whole request, so a deployment that holds such a resource shows it too.
    pub stored_only: &'a std::sync::atomic::AtomicBool,
}

impl Cx<'_> {
    /// Remember that this request touched a stored-only resource (see the field).
    pub fn mark_stored_only(&self) {
        self.stored_only.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// `:4577` taken from the Host the client used, or empty when it sent no port.
    pub fn port_suffix(&self) -> String {
        crate::util::port_suffix_of(&self.req.ctx.endpoint.authority)
    }

    /// `http://acct.blob.localhost:4577` style URL of a data plane host.
    pub fn data_url(&self, scheme: &str, account: &str, label: &str) -> String {
        format!("{scheme}://{account}.{label}.localhost{}", self.port_suffix())
    }

    /// Storage hosts carry `.core.` so the Python storage SDK does not take them for Azurite.
    pub fn storage_url(&self, scheme: &str, account: &str, label: &str) -> String {
        format!("{scheme}://{account}.{label}.core.localhost{}", self.port_suffix())
    }

    /// `https://management.localhost:4577`, the address the client used for ARM.
    pub fn arm_base(&self) -> String {
        format!("{}://{}", self.req.ctx.endpoint.scheme, self.req.ctx.endpoint.authority)
    }

    /// 202 with a status URL that answers 200 at once (the SDK pollers wait for it).
    pub fn accepted(&self) -> azemu_core::Response {
        let segs = self.req.segments();
        let sub = if segs.len() >= 2 && segs[0].eq_ignore_ascii_case("subscriptions") {
            segs[1].as_str()
        } else {
            "00000000-0000-0000-0000-000000000000"
        };
        let api = self.req.query.get("api-version").unwrap_or("2020-06-01");
        let url = format!("{}/subscriptions/{sub}/operationresults/{}?api-version={api}", self.arm_base(), uuid::Uuid::new_v4().simple());
        azemu_core::resp::Resp::new(202u16).header("Location", url).header("Retry-After", "0").build()
    }

    pub fn now_iso(&self) -> String {
        iso8601_7(self.now)
    }
}
