//! Leases on blobs and containers: the state machine and the Lease Blob /
//! Lease Container operations.

use azemu_core::resp::Resp;
use azemu_core::time::{self, Duration};
use azemu_core::Request;

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::errors::{blob_not_found, err, ApiError, ApiResult};
use crate::state::{Lease, LeaseState, Time};
use crate::util::{hdr, hdr_i64};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Blob,
    Container,
}

impl Scope {
    fn word(self) -> &'static str {
        match self {
            Scope::Blob => "blob",
            Scope::Container => "container",
        }
    }
}

fn lease_err(code: &str, scope: Scope) -> ApiError {
    let w = scope.word();
    let e = ApiError::new(code);
    match code {
        "LeaseIdMissing" => e.msg(format!("There is currently a lease on the {w} and no lease ID was specified in the request.")),
        "LeaseLost" => e.msg(format!("A lease ID was specified, but the lease for the {w} has expired.")),
        "LeaseNotPresentWithLeaseOperation" => e.msg(format!("There is currently no lease on the {w}.")),
        "LeaseIdMismatchWithLeaseOperation" => e.msg(format!("The lease ID specified did not match the lease ID for the {w}.")),
        _ => e,
    }
}

fn mismatch(scope: Scope) -> ApiError {
    match scope {
        Scope::Blob => ApiError::new("LeaseIdMismatchWithBlobOperation"),
        Scope::Container => ApiError::new("LeaseIdMismatchWithContainerOperation"),
    }
}

fn not_present(scope: Scope) -> ApiError {
    match scope {
        Scope::Blob => ApiError::new("LeaseNotPresentWithBlobOperation"),
        Scope::Container => ApiError::new("LeaseNotPresentWithContainerOperation"),
    }
}

/// A write needs the lease id while the resource is leased. `given` is `x-ms-lease-id`.
/// The lease must already be refreshed.
pub fn check_write(lease: &Lease, given: Option<&str>, scope: Scope) -> Result<(), ApiError> {
    if let Some(g) = given {
        if !valid_guid(g) {
            return Err(ApiError::invalid_header("x-ms-lease-id", g));
        }
    }
    match (lease.state, given) {
        (LeaseState::Leased | LeaseState::Breaking, None) => Err(lease_err("LeaseIdMissing", scope)),
        (LeaseState::Leased | LeaseState::Breaking, Some(g)) => {
            if lease.id.as_deref() == Some(g) {
                Ok(())
            } else {
                Err(mismatch(scope))
            }
        }
        (LeaseState::Expired, Some(g)) => {
            if lease.id.as_deref() == Some(g) {
                Err(lease_err("LeaseLost", scope))
            } else {
                Err(not_present(scope))
            }
        }
        (LeaseState::Available | LeaseState::Broken, Some(_)) => Err(not_present(scope)),
        (_, None) => Ok(()),
    }
}

/// A read only looks at a lease id when one is given.
pub fn check_read(lease: &Lease, given: Option<&str>, scope: Scope) -> Result<(), ApiError> {
    match given {
        None => Ok(()),
        Some(_) => check_write(lease, given, scope),
    }
}

/// What a lease action did.
pub enum Outcome {
    Acquired(String),
    Renewed(String),
    Changed(String),
    Released,
    Broken(i64),
}

pub struct Action<'a> {
    pub action: &'a str,
    pub lease_id: Option<&'a str>,
    pub proposed: Option<&'a str>,
    pub duration: Option<i64>,
    pub break_period: Option<i64>,
}

fn valid_guid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

fn remaining_secs(until: Time, now: Time) -> i64 {
    let ms = (until - now).whole_milliseconds();
    if ms <= 0 {
        0
    } else {
        ((ms + 999) / 1000) as i64
    }
}

/// Apply a lease action to a (refreshed) lease.
pub fn apply(lease: &mut Lease, a: &Action<'_>, now: Time, scope: Scope) -> Result<Outcome, ApiError> {
    let not_present_lease = || lease_err("LeaseNotPresentWithLeaseOperation", scope);
    let id_mismatch = || lease_err("LeaseIdMismatchWithLeaseOperation", scope);
    match a.action.to_ascii_lowercase().as_str() {
        "acquire" => {
            let dur = a.duration.ok_or_else(|| ApiError::missing_header("x-ms-lease-duration"))?;
            if dur != -1 && !(15..=60).contains(&dur) {
                return Err(ApiError::invalid_header("x-ms-lease-duration", &dur.to_string()));
            }
            if let Some(p) = a.proposed {
                if !valid_guid(p) {
                    return Err(ApiError::invalid_header("x-ms-proposed-lease-id", p));
                }
            }
            match lease.state {
                LeaseState::Leased | LeaseState::Breaking => {
                    if a.proposed.is_some() && lease.id.as_deref() == a.proposed {
                        if lease.state == LeaseState::Breaking {
                            return Err(err("LeaseIsBreakingAndCannotBeAcquired"));
                        }
                    } else {
                        return Err(err("LeaseAlreadyPresent"));
                    }
                }
                _ => {
                    lease.id = Some(a.proposed.map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
                }
            }
            lease.state = LeaseState::Leased;
            lease.infinite = dur == -1;
            lease.secs = if dur == -1 { 0 } else { dur as u32 };
            lease.expires = if dur == -1 { None } else { Some(now + Duration::seconds(dur)) };
            Ok(Outcome::Acquired(lease.id.clone().unwrap_or_default()))
        }
        "renew" => {
            let id = a.lease_id.ok_or_else(|| ApiError::missing_header("x-ms-lease-id"))?;
            match lease.state {
                LeaseState::Available => Err(not_present_lease()),
                _ if lease.id.as_deref() != Some(id) => Err(id_mismatch()),
                LeaseState::Leased | LeaseState::Expired => {
                    lease.state = LeaseState::Leased;
                    lease.expires = if lease.infinite { None } else { Some(now + Duration::seconds(lease.secs as i64)) };
                    Ok(Outcome::Renewed(id.to_string()))
                }
                LeaseState::Breaking | LeaseState::Broken => Err(err("LeaseIsBrokenAndCannotBeRenewed")),
            }
        }
        "change" => {
            let id = a.lease_id.ok_or_else(|| ApiError::missing_header("x-ms-lease-id"))?;
            let new = a.proposed.ok_or_else(|| ApiError::missing_header("x-ms-proposed-lease-id"))?;
            if !valid_guid(new) {
                return Err(ApiError::invalid_header("x-ms-proposed-lease-id", new));
            }
            match lease.state {
                LeaseState::Available | LeaseState::Expired | LeaseState::Broken => Err(not_present_lease()),
                LeaseState::Leased | LeaseState::Breaking => {
                    if lease.id.as_deref() != Some(id) && lease.id.as_deref() != Some(new) {
                        return Err(id_mismatch());
                    }
                    if lease.state == LeaseState::Breaking {
                        return Err(err("LeaseIsBreakingAndCannotBeChanged"));
                    }
                    lease.id = Some(new.to_string());
                    Ok(Outcome::Changed(new.to_string()))
                }
            }
        }
        "release" => {
            let id = a.lease_id.ok_or_else(|| ApiError::missing_header("x-ms-lease-id"))?;
            match lease.state {
                LeaseState::Available => Err(not_present_lease()),
                _ if lease.id.as_deref() != Some(id) => Err(id_mismatch()),
                _ => {
                    *lease = Lease::default();
                    Ok(Outcome::Released)
                }
            }
        }
        "break" => {
            if let Some(p) = a.break_period {
                if !(0..=60).contains(&p) {
                    return Err(ApiError::invalid_header("x-ms-lease-break-period", &p.to_string()));
                }
            }
            match lease.state {
                LeaseState::Available | LeaseState::Expired => Err(not_present_lease()),
                LeaseState::Broken => Ok(Outcome::Broken(0)),
                LeaseState::Leased => {
                    let remaining = if lease.infinite { None } else { lease.expires.map(|e| remaining_secs(e, now)) };
                    let secs = match (a.break_period, remaining) {
                        (Some(p), Some(r)) => p.min(r),
                        (Some(p), None) => p,
                        (None, Some(r)) => r,
                        (None, None) => 0,
                    };
                    if secs == 0 {
                        lease.state = LeaseState::Broken;
                        lease.expires = None;
                    } else {
                        lease.state = LeaseState::Breaking;
                        lease.expires = Some(now + Duration::seconds(secs));
                    }
                    Ok(Outcome::Broken(secs))
                }
                LeaseState::Breaking => {
                    let cur = lease.expires.map(|e| remaining_secs(e, now)).unwrap_or(0);
                    let secs = a.break_period.map(|p| p.min(cur)).unwrap_or(cur);
                    if secs == 0 {
                        lease.state = LeaseState::Broken;
                        lease.expires = None;
                    } else {
                        lease.expires = Some(now + Duration::seconds(secs));
                    }
                    Ok(Outcome::Broken(secs))
                }
            }
        }
        _ => Err(ApiError::invalid_header("x-ms-lease-action", a.action)),
    }
}

fn parse_action(req: &Request) -> Result<Action<'_>, ApiError> {
    let action = hdr(req, "x-ms-lease-action").ok_or_else(|| ApiError::missing_header("x-ms-lease-action"))?;
    Ok(Action {
        action,
        lease_id: hdr(req, "x-ms-lease-id"),
        proposed: hdr(req, "x-ms-proposed-lease-id"),
        duration: hdr_i64(req, "x-ms-lease-duration")?,
        break_period: hdr_i64(req, "x-ms-lease-break-period")?,
    })
}

fn respond(out: Outcome, etag: &str, lm: Time) -> azemu_core::Response {
    let (status, r) = match out {
        Outcome::Acquired(id) => (201u16, Resp::new(201u16).header("x-ms-lease-id", id)),
        Outcome::Renewed(id) | Outcome::Changed(id) => (200, Resp::new(200u16).header("x-ms-lease-id", id)),
        Outcome::Released => (200, Resp::new(200u16)),
        Outcome::Broken(t) => (202, Resp::new(202u16).header("x-ms-lease-time", t)),
    };
    let _ = status;
    r.header("ETag", etag).header("Last-Modified", time::rfc1123(lm)).build()
}

pub fn lease_blob(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let action = parse_action(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    let out = apply(&mut b.lease, &action, now, Scope::Blob)?;
    Ok(respond(out, &b.etag, b.last_modified))
}

pub fn lease_container(cx: &mut Cx, container: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let action = parse_action(req)?;
    let c = cx.container(container)?;
    cond::check(req, "", Some((&c.etag, c.last_modified)), Kind::Write)?;
    let out = apply(&mut c.lease, &action, now, Scope::Container)?;
    Ok(respond(out, &c.etag, c.last_modified))
}
