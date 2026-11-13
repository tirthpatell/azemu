//! The namespace document, `$namespaceinfo`, and the "unknown entity" answer.

use std::sync::OnceLock;

use azemu_core::Response;

use super::render::{self, CT_ENTRY, CT_FEED, SB_NS, XSI_NS};
use super::util::{esc, ts_secs, ts_trimmed};
use super::Cx;

/// When this process first answered a namespace request: stands in for the namespace's creation time.
fn created_ms(cx: &Cx) -> i64 {
    static CREATED: OnceLock<i64> = OnceLock::new();
    *CREATED.get_or_init(|| cx.now)
}

/// `GET /`: the list of publicly listed services (also what the service answers for an unknown entity).
pub fn root_feed(cx: &Cx) -> Response {
    let body = format!(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\"><title type=\"text\">Publicly Listed Services</title>\
         <subtitle type=\"text\">This is the list of publicly-listed services currently available.</subtitle>\
         <updated>{}</updated><generator>Service Bus 1.1</generator></feed>",
        ts_secs(cx.now)
    );
    render::respond(200, CT_FEED, None, body)
}

/// `GET /$namespaceinfo`
pub fn namespace_info(cx: &Cx) -> Response {
    let name = esc(&cx.account);
    let url = esc(&cx.url());
    let created = ts_trimmed(created_ms(cx));
    let body = format!(
        "<entry xmlns=\"http://www.w3.org/2005/Atom\"><id>{url}</id><title type=\"text\">{name}</title><updated>{}</updated>\
         <author><name>{name}</name></author><link rel=\"self\" href=\"{url}\"/><content type=\"application/xml\">\
         <NamespaceInfo xmlns=\"{SB_NS}\" xmlns:i=\"{XSI_NS}\"><CreatedTime>{created}</CreatedTime><MessagingSKU>Standard</MessagingSKU>\
         <ModifiedTime>{created}</ModifiedTime><Name>{name}</Name><NamespaceType>Messaging</NamespaceType></NamespaceInfo></content></entry>",
        ts_secs(cx.now)
    );
    render::respond(200, CT_ENTRY, None, body)
}
