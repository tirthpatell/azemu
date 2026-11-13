//! Conditional request headers (RFC 7232) the way Azure Storage applies them.
//!
//! Evaluation order (RFC 7232 section 6):
//!
//! 1. `If-Match` present: no match gives `PreconditionFailed`.
//!    Otherwise, `If-Unmodified-Since` (only when no `If-Match`): modified after
//!    the date gives `PreconditionFailed`.
//! 2. `If-None-Match` present: a match gives `NotModified` for reads and
//!    `PreconditionFailed` for writes. Otherwise, `If-Modified-Since` (only when no
//!    `If-None-Match`): not modified since the date gives `NotModified` for reads
//!    and `PreconditionFailed` for writes.
//!
//! Dates compare at second precision. A date that does not parse is ignored.
//! Weak etags compare by opaque value.
//!
//! A resource that does not exist ([`Resource::Missing`]): `If-Match`,
//! `If-Modified-Since` and `If-Unmodified-Since` cannot be satisfied and give
//! `PreconditionFailed`; `If-None-Match` (including `*`) passes. Services that
//! return 404 for a missing resource on reads should do that before calling this.

use http::HeaderMap;
use time::OffsetDateTime;

use crate::etag::EtagMatch;
use crate::time::{parse_rfc1123, truncate_to_secs};

/// The four conditional headers as sent by the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Conditions<'a> {
    pub if_match: Option<&'a str>,
    pub if_none_match: Option<&'a str>,
    pub if_modified_since: Option<&'a str>,
    pub if_unmodified_since: Option<&'a str>,
}

impl<'a> Conditions<'a> {
    /// Read the four headers from a header map (non-UTF-8 values count as absent).
    pub fn from_headers(h: &'a HeaderMap) -> Conditions<'a> {
        let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok());
        Conditions {
            if_match: get("if-match"),
            if_none_match: get("if-none-match"),
            if_modified_since: get("if-modified-since"),
            if_unmodified_since: get("if-unmodified-since"),
        }
    }

    /// True when none of the four headers is present.
    pub fn is_empty(&self) -> bool {
        self.if_match.is_none() && self.if_none_match.is_none() && self.if_modified_since.is_none() && self.if_unmodified_since.is_none()
    }

    /// Evaluate against the current state of the resource.
    pub fn evaluate(&self, resource: Resource<'_>, access: Access) -> Outcome {
        let (etag, modified) = match resource {
            Resource::Missing => return self.evaluate_missing(),
            Resource::Exists { etag, last_modified } => (etag, truncate_to_secs(last_modified)),
        };

        if let Some(im) = self.if_match {
            if !EtagMatch::parse(im).matches(etag) {
                return Outcome::PreconditionFailed;
            }
        } else if let Some(d) = self.if_unmodified_since.and_then(parse_rfc1123) {
            if modified > truncate_to_secs(d) {
                return Outcome::PreconditionFailed;
            }
        }

        let failed = match access {
            Access::Read => Outcome::NotModified,
            Access::Write => Outcome::PreconditionFailed,
        };
        if let Some(inm) = self.if_none_match {
            if EtagMatch::parse(inm).matches(etag) {
                return failed;
            }
        } else if let Some(d) = self.if_modified_since.and_then(parse_rfc1123) {
            if modified <= truncate_to_secs(d) {
                return failed;
            }
        }
        Outcome::Proceed
    }

    fn evaluate_missing(&self) -> Outcome {
        if self.if_match.is_some()
            || self.if_unmodified_since.and_then(parse_rfc1123).is_some()
            || self.if_modified_since.and_then(parse_rfc1123).is_some()
        {
            Outcome::PreconditionFailed
        } else {
            Outcome::Proceed
        }
    }
}

/// Current state of the resource the request is about.
#[derive(Debug, Clone, Copy)]
pub enum Resource<'a> {
    /// Does not exist.
    Missing,
    Exists {
        /// With or without quotes, weak or strong.
        etag: &'a str,
        last_modified: OffsetDateTime,
    },
}

/// Whether the operation reads (GET/HEAD) or changes the resource. Azure answers a
/// failed `If-None-Match` / `If-Modified-Since` with 304 for reads and 412 for writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Proceed,
    /// Answer 304.
    NotModified,
    /// Answer 412.
    PreconditionFailed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const LM: OffsetDateTime = datetime!(2022-10-27 22:06:34.5 UTC);
    const E: &str = "\"0xABC\"";

    fn res() -> Resource<'static> {
        Resource::Exists { etag: E, last_modified: LM }
    }

    fn eval(c: Conditions<'_>, a: Access) -> Outcome {
        c.evaluate(res(), a)
    }

    #[test]
    fn no_conditions_proceed() {
        assert!(Conditions::default().is_empty());
        assert_eq!(eval(Conditions::default(), Access::Read), Outcome::Proceed);
        assert_eq!(Conditions::default().evaluate(Resource::Missing, Access::Write), Outcome::Proceed);
    }

    #[test]
    fn if_match() {
        let c = |v| Conditions { if_match: Some(v), ..Default::default() };
        for a in [Access::Read, Access::Write] {
            assert_eq!(eval(c("\"0xABC\""), a), Outcome::Proceed);
            assert_eq!(eval(c("*"), a), Outcome::Proceed);
            assert_eq!(eval(c("\"zzz\", \"0xABC\""), a), Outcome::Proceed);
            assert_eq!(eval(c("W/\"0xABC\""), a), Outcome::Proceed);
            assert_eq!(eval(c("0xABC"), a), Outcome::Proceed);
            assert_eq!(eval(c("\"zzz\""), a), Outcome::PreconditionFailed);
            assert_eq!(eval(c("\"zzz\",\"yyy\""), a), Outcome::PreconditionFailed);
            assert_eq!(c("*").evaluate(Resource::Missing, a), Outcome::PreconditionFailed);
        }
    }

    #[test]
    fn if_none_match() {
        let c = |v| Conditions { if_none_match: Some(v), ..Default::default() };
        assert_eq!(eval(c("\"0xABC\""), Access::Read), Outcome::NotModified);
        assert_eq!(eval(c("*"), Access::Read), Outcome::NotModified);
        assert_eq!(eval(c("W/\"0xABC\""), Access::Read), Outcome::NotModified);
        assert_eq!(eval(c("\"0xABC\""), Access::Write), Outcome::PreconditionFailed);
        assert_eq!(eval(c("*"), Access::Write), Outcome::PreconditionFailed);
        assert_eq!(eval(c("\"other\", \"x\""), Access::Read), Outcome::Proceed);
        assert_eq!(eval(c("\"other\""), Access::Write), Outcome::Proceed);
        // create-only semantic: * on a missing resource passes
        assert_eq!(c("*").evaluate(Resource::Missing, Access::Write), Outcome::Proceed);
        assert_eq!(c("\"a\"").evaluate(Resource::Missing, Access::Read), Outcome::Proceed);
    }

    #[test]
    fn if_modified_since() {
        let c = |v| Conditions { if_modified_since: Some(v), ..Default::default() };
        // LM is 22:06:34 (second precision), so a date equal to it means "not modified"
        let same = "Thu, 27 Oct 2022 22:06:34 GMT";
        let before = "Thu, 27 Oct 2022 22:06:33 GMT";
        let after = "Thu, 27 Oct 2022 22:06:35 GMT";
        assert_eq!(eval(c(same), Access::Read), Outcome::NotModified);
        assert_eq!(eval(c(after), Access::Read), Outcome::NotModified);
        assert_eq!(eval(c(before), Access::Read), Outcome::Proceed);
        assert_eq!(eval(c(same), Access::Write), Outcome::PreconditionFailed);
        assert_eq!(eval(c(before), Access::Write), Outcome::Proceed);
        assert_eq!(eval(c("not a date"), Access::Read), Outcome::Proceed);
    }

    #[test]
    fn if_unmodified_since() {
        let c = |v| Conditions { if_unmodified_since: Some(v), ..Default::default() };
        let same = "Thu, 27 Oct 2022 22:06:34 GMT";
        let before = "Thu, 27 Oct 2022 22:06:33 GMT";
        let after = "Thu, 27 Oct 2022 22:06:35 GMT";
        for a in [Access::Read, Access::Write] {
            assert_eq!(eval(c(same), a), Outcome::Proceed);
            assert_eq!(eval(c(after), a), Outcome::Proceed);
            assert_eq!(eval(c(before), a), Outcome::PreconditionFailed);
            assert_eq!(eval(c("junk"), a), Outcome::Proceed);
        }
    }

    #[test]
    fn missing_resource() {
        let m = Resource::Missing;
        let d = Some("Thu, 27 Oct 2022 22:06:34 GMT");
        assert_eq!(Conditions { if_unmodified_since: d, ..Default::default() }.evaluate(m, Access::Write), Outcome::PreconditionFailed);
        assert_eq!(Conditions { if_modified_since: d, ..Default::default() }.evaluate(m, Access::Write), Outcome::PreconditionFailed);
        assert_eq!(Conditions { if_modified_since: Some("junk"), ..Default::default() }.evaluate(m, Access::Write), Outcome::Proceed);
    }

    #[test]
    fn combinations_follow_rfc_7232() {
        let before = Some("Thu, 27 Oct 2022 22:06:33 GMT");
        // If-Match true wins over a failing If-Unmodified-Since (which is ignored)
        let c = Conditions { if_match: Some("*"), if_unmodified_since: before, ..Default::default() };
        assert_eq!(eval(c, Access::Write), Outcome::Proceed);
        // If-Match false fails first
        let c = Conditions { if_match: Some("\"no\""), if_none_match: Some("\"also-no\""), ..Default::default() };
        assert_eq!(eval(c, Access::Read), Outcome::PreconditionFailed);
        // If-None-Match present: If-Modified-Since is ignored
        let c =
            Conditions { if_none_match: Some("\"other\""), if_modified_since: Some("Fri, 28 Oct 2022 00:00:00 GMT"), ..Default::default() };
        assert_eq!(eval(c, Access::Read), Outcome::Proceed);
        // If-Match ok and If-None-Match matching: read gives 304
        let c = Conditions { if_match: Some("*"), if_none_match: Some(E), ..Default::default() };
        assert_eq!(eval(c, Access::Read), Outcome::NotModified);
        assert_eq!(eval(c, Access::Write), Outcome::PreconditionFailed);
    }

    #[test]
    fn from_headers_reads_all_four() {
        let mut h = HeaderMap::new();
        h.insert("If-Match", "\"a\"".parse().unwrap());
        h.insert("if-none-match", "*".parse().unwrap());
        h.insert("If-Modified-Since", "x".parse().unwrap());
        h.insert("if-unmodified-since", "y".parse().unwrap());
        let c = Conditions::from_headers(&h);
        assert_eq!(c.if_match, Some("\"a\""));
        assert_eq!(c.if_none_match, Some("*"));
        assert_eq!(c.if_modified_since, Some("x"));
        assert_eq!(c.if_unmodified_since, Some("y"));
        assert!(!c.is_empty());
        assert!(Conditions::from_headers(&HeaderMap::new()).is_empty());
    }
}
