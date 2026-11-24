//! Strict authorisation for App Configuration (`docs/strict-auth.md`): a bearer token is checked and the roles of the principal decide. A request
//! that is refused has an empty body and no `Content-Type`: 403 for a missing role, 401 with `WWW-Authenticate: HMAC-SHA256, Bearer
//! error="invalid_token", ...` for a token that is wrong (what Azure sent in November 2026). A key or connection string (`HMAC-SHA256`) is not touched.

use azemu_core::auth::{Aud, Auth, Need, Resource};
use azemu_core::resp::Resp;
use azemu_core::{Request, Response};

const KV: &str = "Microsoft.AppConfiguration/configurationStores/keyValues";
const SNAP: &str = "Microsoft.AppConfiguration/configurationStores/snapshots";

fn need_of(req: &Request) -> Need {
    let segs = req.segments();
    let first = segs.first().map(String::as_str).unwrap_or("");
    let m = req.method.as_str();
    let (base, read) = if first.starts_with("snapshot") { (SNAP, "read") } else { (KV, "read") };
    match (m, first) {
        ("GET" | "HEAD", _) => Need::data(&format!("{base}/{read}")),
        ("PUT" | "DELETE", "locks") => Need::data(&format!("{KV}/action")),
        ("PUT", "snapshots") => Need::data(&format!("{SNAP}/write")),
        ("PATCH", "snapshots") => Need::data(&format!("{SNAP}/archive/action")),
        ("DELETE", _) => Need::data(&format!("{KV}/delete")),
        _ => Need::data(&format!("{KV}/write")),
    }
}

/// The domains a store's own URL is under (`docs/strict-auth.md`, "Audiences").
const STORE_DOMAINS: &[&str] = &["azconfig.io", "azconfig.azure.cn", "azconfig.azure.us", "azconfig.io.localhost", "azconfig.localhost"];

/// `Some(answer)` when the request is refused.
pub fn gate(auth: &Auth, req: &Request) -> Option<Response> {
    if !auth.applies(req) {
        return None;
    }
    let store = req.account.to_ascii_lowercase();
    // `https://azconfig.io` and `https://appconfig.azure.com` are what Azure takes; the SDK asks for the domain of the endpoint it was given
    // (`https://azconfig.localhost` for `https://store.azconfig.localhost:4577`), so that is taken too
    let host = req.ctx.endpoint.authority.split(':').next().unwrap_or("");
    let domain = host.split_once('.').map(|(_, d)| d).unwrap_or(host);
    // and the store's own URL: its host is the store under an App Configuration domain (the Azure clouds', or the emulator's)
    let auds = [
        Aud::exact("https://azconfig.io"),
        Aud::exact("https://appconfig.azure.com"),
        Aud::exact(&format!("https://{domain}")),
        Aud::host_in(&store, STORE_DOMAINS),
    ];
    let who = match auth.authenticate(req, &auds) {
        Ok(w) => w,
        Err(_) => {
            return Some(
                Resp::new(401u16)
                    .header(
                        "WWW-Authenticate",
                        "HMAC-SHA256, Bearer error=\"invalid_token\", error_description=\"Authorization token failed validation\"",
                    )
                    .build(),
            )
        }
    };
    let what = format!("{} {}", req.method, req.decoded_path());
    auth.authorize(&what, &who, &need_of(req), &Resource::config_store(&store)).err().map(|_| Resp::new(403u16).build())
}
