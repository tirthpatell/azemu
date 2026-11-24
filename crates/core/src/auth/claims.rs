//! What the credentials put on one AMQP connection give it (Service Bus and Event Hubs, strict mode).
//!
//! `$cbs` `put-token` hands the broker a credential for an audience (an entity address). The broker answers 202 whenever the credential
//! itself is good; whether the connection may *send* or *receive* is decided later, at the link attach, from what the connection holds
//! ([`ConnClaims::covering`]). A token has a lifetime: when it runs out without a newer one for that audience, the links that depended
//! on it are closed. The table is shared by the two services of one connection (the multiplexer may hand a link to the other one),
//! found by the id of the connection ([`ConnClaims::for_conn`]).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;

use super::sas::Rights;
use super::token::Principal;

/// Who a grant is for.
#[derive(Debug, Clone)]
pub enum Holder {
    /// A shared access signature: the rights of its rule.
    Rule { name: String, rights: Rights },
    /// An Entra token: what the roles of the principal say, asked at the attach.
    Principal(Principal),
}

/// One credential that was put for an audience.
#[derive(Debug, Clone)]
pub struct Grant {
    /// The audience path, lower case, no outer slashes (`q`, `t/subscriptions/s`; empty for the namespace).
    pub audience: String,
    pub holder: Holder,
    /// Unix seconds. For a SAS token the expiry plus the grace.
    pub expires: i64,
}

/// The most credentials one connection holds at once.
pub const MAX_GRANTS: usize = 1000;
/// The longest audience (the `name` of a `put-token`) that is kept.
pub const MAX_AUDIENCE_BYTES: usize = 1024;

/// The connection already holds [`MAX_GRANTS`] credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyGrants;

type Detach = Box<dyn Fn() + Send + Sync>;

struct LinkEntry {
    path: String,
    detach: Detach,
}

#[derive(Default)]
struct Inner {
    grants: Vec<Grant>,
    links: HashMap<u64, LinkEntry>,
    watcher: bool,
}

/// The table of one connection.
pub struct ConnClaims {
    id: usize,
    inner: Mutex<Inner>,
    wake: Arc<Notify>,
}

fn registry() -> &'static Mutex<HashMap<usize, Weak<ConnClaims>>> {
    static R: OnceLock<Mutex<HashMap<usize, Weak<ConnClaims>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Drop for ConnClaims {
    fn drop(&mut self) {
        let mut r = registry().lock();
        if r.get(&self.id).is_some_and(|w| w.strong_count() == 0) {
            r.remove(&self.id);
        }
        self.wake.notify_one();
    }
}

impl ConnClaims {
    /// The table of the connection with this id (`ConnHandle::id`); the same one for every caller while the connection lives.
    pub fn for_conn(id: usize) -> Arc<ConnClaims> {
        let mut r = registry().lock();
        if let Some(c) = r.get(&id).and_then(Weak::upgrade) {
            return c;
        }
        let c = Arc::new(ConnClaims { id, inner: Mutex::default(), wake: Arc::new(Notify::new()) });
        r.insert(id, Arc::downgrade(&c));
        c
    }

    /// Put a credential. One for the same audience is replaced (a renewal). A connection holds at most [`MAX_GRANTS`] credentials that have not
    /// run out (an emulator limit: the audience is the client's text, and a loop that puts a token for a new name each time would grow without end).
    pub fn put(self: &Arc<Self>, grant: Grant) -> Result<(), TooManyGrants> {
        let start = {
            let mut g = self.inner.lock();
            g.grants.retain(|x| x.audience != grant.audience);
            if g.grants.len() >= MAX_GRANTS {
                let now = now_secs();
                g.grants.retain(|x| x.expires > now);
                if g.grants.len() >= MAX_GRANTS {
                    return Err(TooManyGrants);
                }
            }
            g.grants.push(grant);
            !std::mem::replace(&mut g.watcher, true)
        };
        if start {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let weak = Arc::downgrade(self);
                rt.spawn(watch(weak));
            }
        } else {
            self.wake.notify_one();
        }
        Ok(())
    }

    /// The holders of the credentials that cover this path and have not run out.
    pub fn covering(&self, path: &str) -> Vec<Holder> {
        let now = now_secs();
        let p = path.trim_matches('/').to_ascii_lowercase();
        self.inner
            .lock()
            .grants
            .iter()
            .filter(|g| g.expires > now && super::sas::covers(&g.audience, &p))
            .map(|g| g.holder.clone())
            .collect()
    }

    /// Does the connection hold any credential at all that has not run out?
    pub fn has_any(&self) -> bool {
        let now = now_secs();
        self.inner.lock().grants.iter().any(|g| g.expires > now)
    }

    /// A link was attached under the credentials that cover `path`: when they all run out it is detached by `detach`.
    pub fn track_link(&self, link: u64, path: &str, detach: Box<dyn Fn() + Send + Sync>) {
        self.inner.lock().links.insert(link, LinkEntry { path: path.trim_matches('/').to_ascii_lowercase(), detach });
    }

    pub fn forget_link(&self, link: u64) {
        self.inner.lock().links.remove(&link);
    }

    /// Drop the credentials that ran out and return what has to be detached because of it.
    fn expire(&self, now: i64) -> (Vec<Detach>, Option<i64>) {
        let mut g = self.inner.lock();
        let before = g.grants.len();
        g.grants.retain(|x| x.expires > now);
        let mut out = Vec::new();
        if g.grants.len() != before {
            let covered: Vec<u64> = g
                .links
                .iter()
                .filter(|(_, l)| !g.grants.iter().any(|x| super::sas::covers(&x.audience, &l.path)))
                .map(|(id, _)| *id)
                .collect();
            for id in covered {
                if let Some(l) = g.links.remove(&id) {
                    out.push(l.detach);
                }
            }
        }
        (out, g.grants.iter().map(|x| x.expires).min())
    }
}

/// Closes the links whose credentials ran out. One task per connection that holds credentials; it ends with the connection.
async fn watch(weak: Weak<ConnClaims>) {
    loop {
        let Some(c) = weak.upgrade() else { return };
        let (to_detach, next) = c.expire(now_secs());
        for d in to_detach {
            d();
        }
        let wait = next.map(|t| Duration::from_secs((t - now_secs()).max(0) as u64) + Duration::from_millis(50));
        let wake = c.wake.clone();
        drop(c);
        let notified = wake.notified();
        match wait {
            Some(w) => {
                let _ = tokio::time::timeout(w, notified).await;
            }
            None => notified.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rule(rights: &[&str]) -> Holder {
        Holder::Rule { name: "r".into(), rights: Rights::parse(rights.iter().copied()).unwrap() }
    }

    #[tokio::test]
    async fn a_grant_covers_its_audience_and_what_is_below() {
        let c = ConnClaims::for_conn(1_000_001);
        c.put(Grant { audience: "q".into(), holder: rule(&["Send"]), expires: now_secs() + 60 }).unwrap();
        assert_eq!(c.covering("Q").len(), 1);
        assert_eq!(c.covering("q/$deadletterqueue").len(), 1);
        assert!(c.covering("other").is_empty());
        // the same connection id gives the same table; a renewal replaces
        let again = ConnClaims::for_conn(1_000_001);
        again.put(Grant { audience: "q".into(), holder: rule(&["Listen"]), expires: now_secs() + 60 }).unwrap();
        assert_eq!(c.covering("q").len(), 1);
        drop(c);
        drop(again);
        assert!(ConnClaims::for_conn(1_000_001).covering("q").is_empty(), "a new connection with the same id starts empty");
    }

    #[tokio::test]
    async fn a_connection_holds_a_limited_number_of_credentials() {
        let c = ConnClaims::for_conn(1_000_010);
        let grant = |name: String, ttl: i64| Grant { audience: name, holder: rule(&["Send"]), expires: now_secs() + ttl };
        for i in 0..MAX_GRANTS {
            c.put(grant(format!("q{i}"), 3600)).unwrap();
        }
        assert_eq!(c.put(grant("one-too-many".into(), 3600)), Err(TooManyGrants));
        assert!(c.covering("one-too-many").is_empty());
        // a renewal of one it holds is not a new one
        c.put(grant("q7".into(), 7200)).unwrap();
        assert_eq!(c.covering("q7").len(), 1);
        // the ones that ran out make room
        let d = ConnClaims::for_conn(1_000_011);
        for i in 0..MAX_GRANTS {
            d.put(grant(format!("q{i}"), -10)).unwrap();
        }
        d.put(grant("fresh".into(), 3600)).unwrap();
        assert_eq!(d.covering("fresh").len(), 1);
    }

    #[tokio::test]
    async fn the_links_of_a_credential_that_ran_out_are_detached_unless_another_one_covers_them() {
        let c = ConnClaims::for_conn(1_000_002);
        let n = Arc::new(AtomicUsize::new(0));
        c.put(Grant { audience: "q".into(), holder: rule(&["Send"]), expires: now_secs() + 1 }).unwrap();
        c.put(Grant { audience: "".into(), holder: rule(&["Send"]), expires: now_secs() + 3600 }).unwrap();
        let m = n.clone();
        c.track_link(
            1,
            "q",
            Box::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
            }),
        );
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert_eq!(n.load(Ordering::SeqCst), 0, "the namespace credential still covers the link");
        let c2 = ConnClaims::for_conn(1_000_003);
        let m = n.clone();
        c2.put(Grant { audience: "q".into(), holder: rule(&["Send"]), expires: now_secs() + 1 }).unwrap();
        c2.track_link(
            1,
            "q",
            Box::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
            }),
        );
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(!c2.has_any());
    }

    #[tokio::test]
    async fn a_renewal_keeps_the_link() {
        let c = ConnClaims::for_conn(1_000_004);
        let n = Arc::new(AtomicUsize::new(0));
        c.put(Grant { audience: "q".into(), holder: rule(&["Send"]), expires: now_secs() + 1 }).unwrap();
        let m = n.clone();
        c.track_link(
            1,
            "q",
            Box::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
            }),
        );
        c.put(Grant { audience: "q".into(), holder: rule(&["Send"]), expires: now_secs() + 3600 }).unwrap();
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }
}
