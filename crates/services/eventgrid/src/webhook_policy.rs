//! Where a webhook may point. The emulator calls out to the user's own webhooks and to nothing else
//! so by default a target must be:
//!
//! * an IP address that is loopback, private (10/8, 172.16/12, 192.168/16, 100.64/10, IPv6 fc00::/7) or the
//!   unspecified address, or an IPv6 link-local address; the IPv4 link-local range 169.254.0.0/16 is left out on
//!   purpose, because cloud metadata services answer there;
//! * a name with no dot (a Docker Compose service name), `localhost` or a name under `.localhost`,
//!   `host.docker.internal` or `gateway.docker.internal`;
//! * a name the emulator's host check serves: equal to or under a suffix from `AZEMU_ALLOW_HOST` or
//!   `AZEMU_TLS_NAME` (the same rule as `crates/core/src/access.rs`, `host_served`, which is private there; only
//!   the environment variables are seen here, not the options `--allow-host` and `--tls-name`);
//! * a host the user named with `AZEMU_ALLOW_WEBHOOK_HOST` / `--allow-webhook-host` (an IP address, a name, or a
//!   name that covers everything under it; `*` allows every host).
//!
//! The rule is checked when a subscription is made (a 400 that names the option) and again when a connection is
//! made: a name can change what it points to, so the addresses it resolves to are checked too, and a public
//! address is refused unless the user named the host. The check at connection time uses the one lookup the
//! connection needs anyway, and connects to the address it checked.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

const DOCKER_HOSTS: [&str; 2] = ["host.docker.internal", "gateway.docker.internal"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookPolicy {
    /// `*` was given: no check at all.
    allow_all: bool,
    /// Names (and everything under them) and IP addresses the user allowed; lower case.
    allowed: Vec<String>,
    /// Suffixes the host check serves (`AZEMU_ALLOW_HOST`, `AZEMU_TLS_NAME`); lower case.
    served: Vec<String>,
}

impl WebhookPolicy {
    /// `allowed`: the entries of `AZEMU_ALLOW_WEBHOOK_HOST`. `served`: the entries of `AZEMU_ALLOW_HOST` and
    /// `AZEMU_TLS_NAME`.
    pub fn new(allowed: &[String], served: &[String]) -> WebhookPolicy {
        let norm = |s: &String| {
            let s = s.trim().to_ascii_lowercase();
            let s = s.strip_prefix("*.").unwrap_or(&s).trim_end_matches('.').to_string();
            s.trim_start_matches('[').trim_end_matches(']').to_string()
        };
        let allow_all = allowed.iter().any(|a| a.trim() == "*");
        WebhookPolicy {
            allow_all,
            allowed: allowed.iter().filter(|a| a.trim() != "*").map(norm).filter(|s| !s.is_empty()).collect(),
            served: served.iter().map(norm).filter(|s| !s.is_empty()).collect(),
        }
    }

    /// Every host is allowed (for tests and embedders that want the old behaviour).
    pub fn allow_all() -> WebhookPolicy {
        WebhookPolicy { allow_all: true, ..Default::default() }
    }

    fn under(list: &[String], host: &str) -> bool {
        list.iter().any(|s| host == s || host.ends_with(&format!(".{s}")))
    }

    /// Did the user name this host (or `*`)? Then its addresses are not checked either.
    fn named(&self, host: &str) -> bool {
        self.allow_all || Self::under(&self.allowed, host)
    }

    fn name_ok(&self, host: &str) -> bool {
        host == "localhost"
            || host.ends_with(".localhost")
            || !host.contains('.')
            || DOCKER_HOSTS.contains(&host)
            || Self::under(&self.served, host)
    }

    /// Is the host of a webhook URL acceptable? No lookup is made. `Err` is the text of a 400.
    pub fn check_host(&self, host: &str) -> Result<(), String> {
        let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        if self.named(&host) {
            return Ok(());
        }
        let ok = match IpAddr::from_str(host.split('%').next().unwrap_or(&host)) {
            Ok(ip) => local_address(ip),
            Err(_) => self.name_ok(&host),
        };
        if ok {
            return Ok(());
        }
        Err(format!(
            "The webhook host '{host}' is not allowed: by default a webhook must be a loopback or private address, a name with no dot, localhost, or a name this emulator serves, so that test events do not leave your machine by accident. Start azemu with --allow-webhook-host {host} (or set AZEMU_ALLOW_WEBHOOK_HOST={host}) to allow this host, or --allow-webhook-host '*' to allow every host."
        ))
    }

    /// The addresses a connection to `host` may use, out of what the name resolved to. A host the user named keeps
    /// all of them; for any other, public addresses are dropped, and when nothing is left the answer is an error.
    pub fn vet_addresses(&self, host: &str, addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
        let h = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        if self.named(&h) {
            return Ok(addrs);
        }
        let first_public = addrs.iter().find(|a| !local_address(a.ip())).copied();
        let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| local_address(a.ip())).collect();
        if ok.is_empty() {
            return Err(match first_public {
                Some(a) => format!(
                    "The webhook host '{h}' points to the public address {} and was not called. Webhooks only go to loopback or private addresses unless you allow the host: start azemu with --allow-webhook-host {h} (or set AZEMU_ALLOW_WEBHOOK_HOST={h}).",
                    a.ip()
                ),
                None => format!("The webhook host '{h}' did not resolve to any address."),
            });
        }
        Ok(ok)
    }
}

/// Loopback, private or unspecified (see the module text).
pub fn local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => local_v4(v),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => local_v4(m),
            None => local_v6(v),
        },
    }
}

fn local_v4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_loopback() || v.is_unspecified() || v.is_private() || (o[0] == 100 && (64..=127).contains(&o[1]))
}

fn local_v6(v: Ipv6Addr) -> bool {
    let s = v.segments();
    v.is_loopback() || v.is_unspecified() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict() -> WebhookPolicy {
        WebhookPolicy::default()
    }

    fn ok(p: &WebhookPolicy, h: &str) -> bool {
        p.check_host(h).is_ok()
    }

    #[test]
    fn local_hosts_pass_by_default() {
        let p = strict();
        for h in [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "[::1]",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.5",
            "100.64.0.1",
            "0.0.0.0",
            "fd12::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "localhost",
            "LOCALHOST",
            "hook.localhost",
            "myservice",
            "host.docker.internal",
            "localhost.",
        ] {
            assert!(ok(&p, h), "{h} should pass");
        }
    }

    #[test]
    fn public_hosts_are_refused_and_the_message_names_the_option() {
        let p = strict();
        for h in [
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "169.254.169.254",
            "2001:db8::1",
            "::ffff:8.8.8.8",
            "example.com",
            "hooks.example.com",
            "webhook.site",
        ] {
            let e = p.check_host(h).expect_err(h);
            assert!(e.contains("--allow-webhook-host") && e.contains("AZEMU_ALLOW_WEBHOOK_HOST"), "{e}");
        }
    }

    #[test]
    fn the_user_can_name_hosts_or_everything() {
        let p = WebhookPolicy::new(&["example.com".into(), "203.0.113.7".into()], &[]);
        assert!(ok(&p, "example.com") && ok(&p, "hooks.example.com") && ok(&p, "203.0.113.7"));
        assert!(!ok(&p, "notexample.com") && !ok(&p, "203.0.113.8"));
        assert!(ok(&WebhookPolicy::new(&["*".into()], &[]), "anything.example.org"));
        assert!(ok(&WebhookPolicy::new(&["*.example.com".into()], &[]), "a.example.com"));
    }

    #[test]
    fn names_the_host_check_serves_pass() {
        let p = WebhookPolicy::new(&[], &["corp.example".into()]);
        assert!(ok(&p, "hook.corp.example") && ok(&p, "corp.example"));
        assert!(!ok(&p, "hook.other.example"));
    }

    #[test]
    fn a_name_that_points_to_a_public_address_is_refused_at_connection_time() {
        let p = strict();
        let public: SocketAddr = "93.184.216.34:80".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let e = p.vet_addresses("sneaky", vec![public]).unwrap_err();
        assert!(e.contains("93.184.216.34") && e.contains("--allow-webhook-host"), "{e}");
        assert_eq!(p.vet_addresses("sneaky", vec![public, local]).unwrap(), vec![local]);
        // a host the user named keeps every address
        let named = WebhookPolicy::new(&["sneaky".into()], &[]);
        assert_eq!(named.vet_addresses("sneaky", vec![public]).unwrap(), vec![public]);
        assert_eq!(WebhookPolicy::allow_all().vet_addresses("x", vec![public]).unwrap(), vec![public]);
    }
}
