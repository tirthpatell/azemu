//! The Lease Share and Lease File operations (`x-ms-lease-action`), and what a held lease does to other operations.

use azemu_core::resp::Resp;
use azemu_core::time::OffsetDateTime;
use azemu_core::{Request, Response};

use crate::errors;
use crate::lease::{Fail, Lease, OpFail};

/// What is leased, for the words of the errors.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Share,
    File,
}

impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Kind::Share => "file share",
            Kind::File => "file",
        }
    }
    fn operation(self) -> &'static str {
        match self {
            Kind::Share => "Container",
            Kind::File => "File",
        }
    }
}

/// Answer for an operation the lease forbids.
pub fn op_error(req: &Request, kind: Kind, f: OpFail) -> Response {
    let ctx = &req.ctx;
    let noun = kind.noun();
    match f {
        OpFail::Missing => errors::precondition(
            ctx,
            "LeaseIdMissing",
            &format!("There is currently a lease on the {noun} and no lease ID was specified in the request."),
        ),
        OpFail::Mismatch => errors::precondition(
            ctx,
            &format!("LeaseIdMismatchWith{}Operation", kind.operation()),
            &format!("The lease ID specified did not match the lease ID for the {noun}."),
        ),
        OpFail::Lost => {
            errors::precondition(ctx, "LeaseLost", &format!("A lease ID was specified, but the lease for the {noun} has expired."))
        }
        OpFail::NotPresent => errors::precondition(
            ctx,
            &format!("LeaseNotPresentWith{}Operation", kind.operation()),
            &format!("There is currently no lease on the {noun}."),
        ),
    }
}

/// Check the lease for an operation that changes the resource (or, for a share, reads it with an ID).
pub fn check_write(req: &Request, kind: Kind, lease: &Lease, now: OffsetDateTime) -> Result<(), Response> {
    lease.check_write(now, req.header("x-ms-lease-id")).map_err(|f| op_error(req, kind, f))
}

pub fn check_read(req: &Request, kind: Kind, lease: &Lease, now: OffsetDateTime) -> Result<(), Response> {
    lease.check_read(now, req.header("x-ms-lease-id")).map_err(|f| op_error(req, kind, f))
}

fn fail(req: &Request, kind: Kind, f: Fail) -> Response {
    let ctx = &req.ctx;
    let noun = kind.noun();
    match f {
        Fail::Mismatch => errors::conflict(
            ctx,
            "LeaseIdMismatchWithLeaseOperation",
            &format!("The lease ID specified did not match the lease ID for the {noun}."),
        ),
        Fail::NotPresent => {
            errors::conflict(ctx, "LeaseNotPresentWithLeaseOperation", &format!("There is currently no lease on the {noun}."))
        }
        Fail::AlreadyPresent => errors::conflict(ctx, "LeaseAlreadyPresent", "There is already a lease present."),
        Fail::BrokenNoRenew => errors::conflict(
            ctx,
            "LeaseIsBrokenAndCannotBeRenewed",
            "The lease ID matched, but the lease has been broken explicitly and cannot be renewed.",
        ),
        Fail::BreakingNoAcquire => errors::conflict(
            ctx,
            "LeaseIsBreakingAndCannotBeAcquired",
            "The lease ID matched, but the lease is currently in breaking state and cannot be acquired until it is broken.",
        ),
        Fail::BreakingNoChange => errors::conflict(
            ctx,
            "LeaseIsBreakingAndCannotBeChanged",
            "The lease ID matched, but the lease is currently in breaking state and cannot be changed.",
        ),
    }
}

/// What the headers ask for.
pub enum Action {
    Acquire { seconds: i64, proposed: Option<String> },
    Renew { id: String },
    Change { id: String, proposed: String },
    Release { id: String },
    Break { id: Option<String>, period: Option<i64> },
}

fn is_guid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

/// Read and check the headers of a lease request (before the resource is looked at: Azure answers a bad header before a missing share).
pub fn parse(req: &Request, kind: Kind) -> Result<Action, Response> {
    let ctx = &req.ctx;
    let header = |n: &str| req.header(n).map(str::trim);
    let required = |n: &str| header(n).filter(|v| !v.is_empty()).map(str::to_string).ok_or_else(|| errors::missing_header(ctx, n));
    let action = header("x-ms-lease-action").ok_or_else(|| errors::missing_header(ctx, ""))?;
    let proposed = |required_header: bool| -> Result<Option<String>, Response> {
        match header("x-ms-proposed-lease-id") {
            Some(p) if !is_guid(p) => Err(errors::invalid_header(ctx, "x-ms-proposed-lease-id", p)),
            Some(p) => Ok(Some(p.to_string())),
            None if required_header => Err(errors::missing_header(ctx, "x-ms-proposed-lease-id")),
            None => Ok(None),
        }
    };
    let lease_id = || -> Result<String, Response> {
        let id = required("x-ms-lease-id")?;
        if !is_guid(&id) {
            return Err(errors::invalid_header(ctx, "x-ms-lease-id", &id));
        }
        Ok(id)
    };
    match action.to_ascii_lowercase().as_str() {
        "acquire" => {
            let d = required("x-ms-lease-duration")?;
            let seconds = match d.parse::<i64>() {
                Ok(-1) => -1,
                Ok(n) if (15..=60).contains(&n) => n,
                _ => return Err(errors::invalid_header(ctx, "x-ms-lease-duration", &d)),
            };
            Ok(Action::Acquire { seconds, proposed: proposed(false)? })
        }
        // A file lease has no renew: Azure refuses the word.
        "renew" if kind == Kind::File => Err(errors::invalid_header(ctx, "x-ms-lease-action", action)),
        "renew" => Ok(Action::Renew { id: lease_id()? }),
        "change" => {
            let id = lease_id()?;
            Ok(Action::Change { id, proposed: proposed(true)?.unwrap_or_default() })
        }
        "release" => Ok(Action::Release { id: lease_id()? }),
        "break" => {
            let period = match header("x-ms-lease-break-period") {
                None => None,
                Some(p) => match p.parse::<i64>() {
                    Ok(n) if (0..=60).contains(&n) => Some(n),
                    _ => return Err(errors::invalid_header(ctx, "x-ms-lease-break-period", p)),
                },
            };
            Ok(Action::Break { id: header("x-ms-lease-id").map(str::to_string), period })
        }
        _ => Err(errors::invalid_header(ctx, "x-ms-lease-action", action)),
    }
}

/// Do it. `etag` and `last_modified` describe the resource (a lease does not change either).
pub fn apply(
    req: &Request,
    kind: Kind,
    action: Action,
    lease: &mut Lease,
    now: OffsetDateTime,
    etag: &str,
    last_modified: OffsetDateTime,
) -> Response {
    let base = |status: u16| Resp::new(status).header("ETag", etag).header("Last-Modified", azemu_core::time::rfc1123(last_modified));
    match action {
        Action::Acquire { seconds, proposed } => match lease.acquire(now, seconds, proposed.as_deref()) {
            Ok(id) => base(201).header("x-ms-lease-id", id).build(),
            Err(f) => fail(req, kind, f),
        },
        Action::Renew { id } => match lease.renew(now, &id) {
            Ok(()) => base(200).header("x-ms-lease-id", id).build(),
            Err(f) => fail(req, kind, f),
        },
        Action::Change { id, proposed } => match lease.change(now, &id, &proposed) {
            Ok(new) => base(200).header("x-ms-lease-id", new).build(),
            Err(f) => fail(req, kind, f),
        },
        Action::Release { id } => match lease.release(now, &id) {
            Ok(()) => base(200).header("x-ms-lease-time", 0).build(),
            Err(f) => fail(req, kind, f),
        },
        Action::Break { id, period } => match lease.break_lease(now, id.as_deref(), period, kind == Kind::File) {
            Ok(wait) => base(202).header("x-ms-lease-time", wait).build(),
            Err(f) => fail(req, kind, f),
        },
    }
}
