//! Leases on shares and files. The rules are the Blob ones with two differences found on Azure in November 2026: a file lease breaks
//! at once (its break period is ignored and the answer says `x-ms-lease-time: 0`) and has no renew, and a share lease can be
//! `breaking` for the break period.

use azemu_core::time::{Duration, OffsetDateTime};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LState {
    #[default]
    Available,
    Leased,
    Expired,
    Breaking,
    Broken,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Lease {
    pub id: Option<String>,
    pub state: LState,
    /// `None`: infinite (while leased).
    pub seconds: Option<i64>,
    /// When a fixed lease ends, or a breaking lease is broken.
    pub until: Option<OffsetDateTime>,
}

/// Why a lease action was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    /// 409 `LeaseIdMismatchWithLeaseOperation`
    Mismatch,
    /// 409 `LeaseNotPresentWithLeaseOperation`
    NotPresent,
    /// 409 `LeaseAlreadyPresent`
    AlreadyPresent,
    /// 409 `LeaseIsBrokenAndCannotBeRenewed`
    BrokenNoRenew,
    /// 409 `LeaseIsBreakingAndCannotBeAcquired`
    BreakingNoAcquire,
    /// 409 `LeaseIsBreakingAndCannotBeChanged`
    BreakingNoChange,
}

/// Why an ordinary operation (write, delete, read of a share) was refused because of the lease.
#[derive(Debug, PartialEq, Eq)]
pub enum OpFail {
    /// 412 `LeaseIdMissing`
    Missing,
    /// 412 `LeaseIdMismatchWith{Container,File}Operation`
    Mismatch,
    /// 412 `LeaseNotPresentWith{Container,File}Operation`
    NotPresent,
    /// 412 `LeaseLost`: the ID is the one of a fixed lease that ran out.
    Lost,
}

impl Lease {
    /// Let time pass: a fixed lease that ended is expired, a breaking one is broken.
    pub fn refresh(&mut self, now: OffsetDateTime) {
        match self.state {
            LState::Leased if self.seconds.is_some() && self.until.is_some_and(|u| u <= now) => {
                self.state = LState::Expired;
                self.until = None;
            }
            LState::Breaking if self.until.is_none_or(|u| u <= now) => {
                self.state = LState::Broken;
                self.until = None;
            }
            _ => {}
        }
    }

    pub fn locked(&self) -> bool {
        matches!(self.state, LState::Leased | LState::Breaking)
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            LState::Available => "available",
            LState::Leased => "leased",
            LState::Expired => "expired",
            LState::Breaking => "breaking",
            LState::Broken => "broken",
        }
    }

    /// `infinite` or `fixed` while the lease is held.
    pub fn duration_name(&self) -> Option<&'static str> {
        self.locked().then_some(if self.seconds.is_some() { "fixed" } else { "infinite" })
    }

    /// The headers of a property read: status, state and duration.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![
            ("x-ms-lease-status", if self.locked() { "locked" } else { "unlocked" }.to_string()),
            ("x-ms-lease-state", self.state_name().to_string()),
        ];
        if let Some(d) = self.duration_name() {
            h.insert(0, ("x-ms-lease-duration", d.to_string()));
        }
        h
    }

    /// The ID holds when there is a lease (held, breaking or just ended and not taken over).
    fn id_is(&self, id: &str) -> bool {
        self.id.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(id))
    }

    /// `seconds` is -1 (infinite) or 15 to 60 (the caller checked).
    pub fn acquire(&mut self, now: OffsetDateTime, seconds: i64, proposed: Option<&str>) -> Result<String, Fail> {
        self.refresh(now);
        match self.state {
            LState::Leased => match proposed {
                Some(p) if self.id_is(p) => {}
                _ => return Err(Fail::AlreadyPresent),
            },
            LState::Breaking => {
                return Err(match proposed {
                    Some(p) if self.id_is(p) => Fail::BreakingNoAcquire,
                    _ => Fail::AlreadyPresent,
                })
            }
            _ => {}
        }
        let id = match (proposed, &self.id) {
            (Some(p), _) => p.to_string(),
            (None, _) => uuid::Uuid::new_v4().to_string(),
        };
        self.id = Some(id.clone());
        self.state = LState::Leased;
        self.seconds = (seconds >= 0).then_some(seconds);
        self.until = (seconds >= 0).then(|| now + Duration::seconds(seconds));
        Ok(id)
    }

    pub fn renew(&mut self, now: OffsetDateTime, id: &str) -> Result<(), Fail> {
        self.refresh(now);
        if !self.id_is(id) || self.state == LState::Available {
            return Err(Fail::Mismatch);
        }
        match self.state {
            LState::Broken => Err(Fail::BrokenNoRenew),
            LState::Breaking => Err(Fail::BrokenNoRenew),
            _ => {
                self.state = LState::Leased;
                if let Some(s) = self.seconds {
                    self.until = Some(now + Duration::seconds(s));
                }
                Ok(())
            }
        }
    }

    pub fn change(&mut self, now: OffsetDateTime, id: &str, proposed: &str) -> Result<String, Fail> {
        self.refresh(now);
        match self.state {
            LState::Available | LState::Broken | LState::Expired => return Err(Fail::NotPresent),
            LState::Breaking if self.id_is(id) => return Err(Fail::BreakingNoChange),
            _ => {}
        }
        if !self.id_is(id) && !self.id_is(proposed) {
            return Err(Fail::Mismatch);
        }
        self.id = Some(proposed.to_string());
        Ok(proposed.to_string())
    }

    pub fn release(&mut self, now: OffsetDateTime, id: &str) -> Result<(), Fail> {
        self.refresh(now);
        if !self.id_is(id) || self.state == LState::Available {
            return Err(Fail::Mismatch);
        }
        *self = Lease::default();
        Ok(())
    }

    /// Break the lease. `period` is `x-ms-lease-break-period` (shares only); `immediate` breaks at once whatever it says (files).
    /// Returns the seconds the lease stays in the way (`x-ms-lease-time`).
    pub fn break_lease(&mut self, now: OffsetDateTime, id: Option<&str>, period: Option<i64>, immediate: bool) -> Result<i64, Fail> {
        self.refresh(now);
        if self.state == LState::Available || (self.state == LState::Broken && !immediate) {
            return Err(Fail::NotPresent);
        }
        if let Some(i) = id {
            if !self.id_is(i) {
                return Err(Fail::Mismatch);
            }
        }
        if immediate {
            // Azure Files answers 202 for a file with no lease too; the file stays free.
            if self.id.is_some() {
                self.state = LState::Broken;
                self.until = None;
                self.seconds = None;
            }
            return Ok(0);
        }
        let remaining = self.until.map(|u| (u - now).whole_seconds().max(0));
        let wait = match (period, remaining) {
            (Some(p), Some(r)) => p.min(r),
            (Some(p), None) => p,
            (None, Some(r)) => r,
            (None, None) => 0,
        };
        if wait == 0 {
            self.state = LState::Broken;
            self.until = None;
        } else {
            self.state = LState::Breaking;
            self.until = Some(now + Duration::seconds(wait));
        }
        Ok(wait)
    }

    /// Is an operation that needs the lease allowed? `given` is `x-ms-lease-id`. A resource with no lease lets a request with no ID
    /// through and refuses one with an ID.
    pub fn check_write(&self, now: OffsetDateTime, given: Option<&str>) -> Result<(), OpFail> {
        let mut me = self.clone();
        me.refresh(now);
        match (me.locked(), given) {
            (true, None) => Err(OpFail::Missing),
            (true, Some(g)) if me.id_is(g) => Ok(()),
            (true, Some(_)) => Err(OpFail::Mismatch),
            (false, Some(g)) if me.state == LState::Expired && me.id_is(g) => Err(OpFail::Lost),
            (false, Some(_)) => Err(OpFail::NotPresent),
            (false, None) => Ok(()),
        }
    }

    /// For a read of a share: the ID is checked when it is given.
    pub fn check_read(&self, now: OffsetDateTime, given: Option<&str>) -> Result<(), OpFail> {
        match given {
            None => Ok(()),
            Some(_) => self.check_write(now, given),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(s)
    }

    #[test]
    fn acquire_is_repeatable_with_the_same_id_only() {
        let mut l = Lease::default();
        assert_eq!(l.acquire(t(0), -1, Some("a")).unwrap(), "a");
        assert_eq!(l.acquire(t(1), -1, Some("a")).unwrap(), "a");
        assert_eq!(l.acquire(t(1), -1, Some("b")), Err(Fail::AlreadyPresent));
        assert_eq!(l.acquire(t(1), -1, None), Err(Fail::AlreadyPresent));
    }

    #[test]
    fn fixed_lease_expires_and_can_be_taken_over() {
        let mut l = Lease::default();
        l.acquire(t(0), 15, Some("a")).unwrap();
        assert!(l.locked());
        l.refresh(t(16));
        assert_eq!(l.state_name(), "expired");
        assert!(l.acquire(t(16), 15, Some("b")).is_ok());
    }

    #[test]
    fn share_break_waits_and_file_break_does_not() {
        let mut l = Lease::default();
        l.acquire(t(0), -1, Some("a")).unwrap();
        assert_eq!(l.break_lease(t(1), None, Some(30), false), Ok(30));
        assert_eq!(l.state_name(), "breaking");
        assert_eq!(l.break_lease(t(1), None, Some(5), false), Ok(5));
        l.refresh(t(7));
        assert_eq!(l.state_name(), "broken");
        let mut f = Lease::default();
        f.acquire(t(0), -1, Some("a")).unwrap();
        assert_eq!(f.break_lease(t(1), None, Some(10), true), Ok(0));
        assert_eq!(f.state_name(), "broken");
    }

    #[test]
    fn release_and_mismatch() {
        let mut l = Lease::default();
        assert_eq!(l.release(t(0), "a"), Err(Fail::Mismatch));
        assert_eq!(l.change(t(0), "a", "b"), Err(Fail::NotPresent));
        assert_eq!(l.break_lease(t(0), None, None, false), Err(Fail::NotPresent));
        l.acquire(t(0), -1, Some("a")).unwrap();
        assert_eq!(l.release(t(0), "b"), Err(Fail::Mismatch));
        assert_eq!(l.change(t(0), "a", "b").unwrap(), "b");
        assert!(l.release(t(0), "b").is_ok());
    }

    #[test]
    fn writes_need_the_id() {
        let mut l = Lease::default();
        assert_eq!(l.check_write(t(0), Some("x")), Err(OpFail::NotPresent));
        l.acquire(t(0), -1, Some("a")).unwrap();
        assert_eq!(l.check_write(t(0), None), Err(OpFail::Missing));
        assert_eq!(l.check_write(t(0), Some("b")), Err(OpFail::Mismatch));
        assert_eq!(l.check_write(t(0), Some("a")), Ok(()));
        assert_eq!(l.check_read(t(0), None), Ok(()));
    }
}
