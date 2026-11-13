//! Per-request context handed to every handler, plus lookup helpers.

use azemu_core::time::parse_iso8601;
use azemu_core::Request;

use crate::auth::Authz;
use crate::errors::{container_not_found, ApiError};
use crate::pre::Pre;
use crate::state::{Account, BlobEntry, Container, State, Time};

pub struct Cx<'a> {
    pub req: &'a Request,
    pub st: &'a mut State,
    pub now: Time,
    /// Account name, lower case.
    pub acct: String,
    pub authz: Authz,
    /// What was worked out before the lock was taken (hashes, a parsed block list, source data).
    pub pre: &'a Pre,
}

impl<'a> Cx<'a> {
    pub fn account(&mut self) -> &mut Account {
        self.st.account(&self.acct)
    }

    /// The container, with its lease brought up to date. 404 ContainerNotFound otherwise.
    pub fn container(&mut self, name: &str) -> Result<&mut Container, ApiError> {
        let now = self.now;
        let c = self.st.account(&self.acct).containers.get_mut(name).ok_or_else(container_not_found)?;
        c.lease.refresh(now);
        Ok(c)
    }
}

/// Bring the leases of the base blob up to date.
pub fn refresh_entry(e: &mut BlobEntry, now: Time) {
    if e.current.as_ref().and_then(|b| b.dfs.as_ref()).and_then(|d| d.expiry).map(|t| t <= now).unwrap_or(false) {
        // The path has expired (Data Lake expiry time): it is gone.
        e.current = None;
    }
    if let Some(b) = e.current.as_mut() {
        b.lease.refresh(now);
    }
}

fn check_time_param(name: &str, v: &str) -> Result<(), ApiError> {
    match parse_iso8601(v) {
        Some(_) => Ok(()),
        None => Err(ApiError::invalid_query(name, v)),
    }
}

/// `snapshot=` query parameter.
pub fn snapshot_param(req: &Request) -> Result<Option<String>, ApiError> {
    match req.query.get("snapshot") {
        None | Some("") => Ok(None),
        Some(v) => {
            check_time_param("snapshot", v)?;
            Ok(Some(v.to_string()))
        }
    }
}

/// `versionid=` query parameter.
pub fn version_param(req: &Request) -> Result<Option<String>, ApiError> {
    match req.query.get("versionid") {
        None | Some("") => Ok(None),
        Some(v) => {
            check_time_param("versionid", v)?;
            Ok(Some(v.to_string()))
        }
    }
}
