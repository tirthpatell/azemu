//! What a running query may use: wall time and the request's cancellation.
//!
//! The evaluator calls [`Budget::work`] for every expression it evaluates, and the scan calls it for every
//! record, so the limit is looked at inside a record, inside a long select list, inside `LIKE`, and in the
//! string functions that handle a big value; it is not looked at only between groups of records. The work is
//! counted in units (one expression node is one unit, a big value costs about one unit per 4 KiB) and the clock
//! is read every [`CHECK_EVERY`] units, so the clock costs next to nothing and a query is stopped within
//! a few milliseconds of its limit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use azemu_core::CancelToken;

use super::like::Like;
use super::value::{EvalError, EvalResult};

/// Units of work between two looks at the clock.
const CHECK_EVERY: u32 = 256;

/// The name of the "error" that stops a query nobody is waiting for any more. It is not sent to anybody.
pub const CANCELLED: &str = "Cancelled";

/// A compiled pattern with the pattern text and escape character it was made from.
type CachedLike = (String, Option<char>, Rc<Like>);

pub struct Budget {
    deadline: Instant,
    seconds: u64,
    cancel: CancelToken,
    units: Cell<u32>,
    /// The last compiled `LIKE` pattern, so a literal pattern is compiled once for all rows.
    like: RefCell<Option<CachedLike>>,
}

impl Budget {
    pub fn new(time: Duration, cancel: CancelToken) -> Budget {
        Budget { deadline: Instant::now() + time, seconds: time.as_secs(), cancel, units: Cell::new(0), like: RefCell::new(None) }
    }

    /// No limit but the 30 s of a unit test, never cancelled.
    #[cfg(test)]
    pub fn unlimited() -> Budget {
        Budget::new(Duration::from_secs(3600), CancelToken::never())
    }

    /// Look at the clock and the cancellation now.
    pub fn check(&self) -> EvalResult<()> {
        if self.cancel.is_cancelled() {
            return Err(EvalError::fatal(CANCELLED, "The request was cancelled."));
        }
        if Instant::now() > self.deadline {
            return Err(EvalError::fatal("QueryTimeout", format!("The query took longer than {} seconds.", self.seconds)));
        }
        Ok(())
    }

    /// Count `units` of work; look at the clock when enough have been done since the last look.
    pub fn work(&self, units: u64) -> EvalResult<()> {
        let n = u64::from(self.units.get()) + units;
        if n >= u64::from(CHECK_EVERY) {
            self.units.set(0);
            return self.check();
        }
        self.units.set(n as u32);
        Ok(())
    }

    /// The work for handling `bytes` bytes of a value: one unit per 4 KiB, at least one.
    pub fn bytes(&self, bytes: usize) -> EvalResult<()> {
        self.work(1 + (bytes / 4096) as u64)
    }

    /// The compiled form of a `LIKE` pattern (the last one is kept).
    pub fn like_pattern(&self, pattern: &str, esc: Option<char>) -> EvalResult<Rc<Like>> {
        if let Some((p, e, l)) = &*self.like.borrow() {
            if p == pattern && *e == esc {
                return Ok(l.clone());
            }
        }
        self.bytes(pattern.len())?;
        let compiled = Rc::new(Like::compile(pattern, esc)?);
        *self.like.borrow_mut() = Some((pattern.to_string(), esc, compiled.clone()));
        Ok(compiled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_reads_the_clock_now_and_then() {
        let b = Budget::new(Duration::ZERO, CancelToken::never());
        std::thread::sleep(Duration::from_millis(2));
        let mut stopped_at = None;
        for i in 0..1000 {
            if b.work(1).is_err() {
                stopped_at = Some(i);
                break;
            }
        }
        let i = stopped_at.expect("the limit was looked at");
        assert!(i < CHECK_EVERY as usize, "after {i} units");
        assert_eq!(b.check().unwrap_err().name, "QueryTimeout");
    }

    #[test]
    fn a_big_value_forces_a_look() {
        let b = Budget::new(Duration::ZERO, CancelToken::never());
        std::thread::sleep(Duration::from_millis(2));
        assert!(b.bytes(4 * 1024 * 1024).is_err());
    }

    #[test]
    fn cancellation_is_an_error_that_names_itself() {
        let t = CancelToken::new();
        let b = Budget::new(Duration::from_secs(60), t.clone());
        assert!(b.check().is_ok());
        t.cancel();
        let e = b.check().unwrap_err();
        assert_eq!((e.name, e.fatal), (CANCELLED, true));
    }
}
