//! Classify how a storage request authenticates. Signatures are not checked.

use crate::time::parse_iso8601;
use crate::Request;
use time::OffsetDateTime;

/// How the request authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // keeps `Sas(SasParams)` as the contract says
pub enum Auth {
    /// `Authorization: SharedKey account:sig` (`lite` for `SharedKeyLite`).
    SharedKey { account: String, lite: bool },
    /// Shared access signature in the query string.
    Sas(SasParams),
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// Nothing usable.
    Anonymous,
}

/// SAS query parameters (percent-decoded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SasParams {
    pub sv: Option<String>,
    pub ss: Option<String>,
    pub srt: Option<String>,
    pub sp: Option<String>,
    pub st: Option<String>,
    pub se: Option<String>,
    pub spr: Option<String>,
    pub sip: Option<String>,
    pub sr: Option<String>,
    pub si: Option<String>,
    pub sig: Option<String>,
    pub skoid: Option<String>,
    pub sktid: Option<String>,
    pub skt: Option<String>,
    pub ske: Option<String>,
    pub sks: Option<String>,
    pub skv: Option<String>,
    pub rscc: Option<String>,
    pub rscd: Option<String>,
    pub rsce: Option<String>,
    pub rscl: Option<String>,
    pub rsct: Option<String>,
}

impl SasParams {
    /// Read the SAS fields from a query.
    pub fn from_query(q: &crate::Query) -> SasParams {
        let g = |n: &str| q.get(n).map(str::to_string);
        SasParams {
            sv: g("sv"),
            ss: g("ss"),
            srt: g("srt"),
            sp: g("sp"),
            st: g("st"),
            se: g("se"),
            spr: g("spr"),
            sip: g("sip"),
            sr: g("sr"),
            si: g("si"),
            sig: g("sig"),
            skoid: g("skoid"),
            sktid: g("sktid"),
            skt: g("skt"),
            ske: g("ske"),
            sks: g("sks"),
            skv: g("skv"),
            rscc: g("rscc"),
            rscd: g("rscd"),
            rsce: g("rsce"),
            rscl: g("rscl"),
            rsct: g("rsct"),
        }
    }

    /// Permissions string (`sp`), "" when absent.
    pub fn permissions(&self) -> &str {
        self.sp.as_deref().unwrap_or("")
    }

    /// True when `se` is present, parses, and is at or before `now`.
    /// A SAS without `se` (expiry comes from a stored access policy) never expires here.
    pub fn expired(&self, now: OffsetDateTime) -> bool {
        match self.se.as_deref().and_then(parse_iso8601) {
            Some(se) => se <= now,
            None => false,
        }
    }

    /// True when `st` is present, parses, and is after `now`.
    pub fn not_yet_valid(&self, now: OffsetDateTime) -> bool {
        match self.st.as_deref().and_then(parse_iso8601) {
            Some(st) => st > now,
            None => false,
        }
    }
}

/// Decide how a request authenticates. An `Authorization` header wins over SAS
/// parameters; a query counts as SAS when it has `sig`.
pub fn classify(req: &Request) -> Auth {
    if let Some(a) = req.header("authorization") {
        let a = a.trim();
        let (scheme, rest) = a.split_once(' ').unwrap_or((a, ""));
        let rest = rest.trim();
        if scheme.eq_ignore_ascii_case("bearer") {
            return Auth::Bearer;
        }
        let lite = scheme.eq_ignore_ascii_case("sharedkeylite");
        if lite || scheme.eq_ignore_ascii_case("sharedkey") {
            let account = rest.split(':').next().unwrap_or("").to_string();
            return Auth::SharedKey { account, lite };
        }
    }
    if req.query.has("sig") {
        return Auth::Sas(SasParams::from_query(&req.query));
    }
    Auth::Anonymous
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Endpoint, Query, RequestCtx};
    use ::time::macros::datetime;
    use bytes::Bytes;

    fn req(auth: Option<&str>, query: &str) -> Request {
        let mut headers = http::HeaderMap::new();
        if let Some(a) = auth {
            headers.insert("authorization", a.parse().unwrap());
        }
        Request {
            method: http::Method::GET,
            path: "/".into(),
            query: Query::parse(query),
            headers,
            body: Bytes::new(),
            account: "a".into(),
            ctx: RequestCtx {
                request_id: "r".into(),
                time: OffsetDateTime::UNIX_EPOCH,
                endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style: false, account: "a".into() },
            },
        }
    }

    #[test]
    fn shared_key() {
        assert_eq!(classify(&req(Some("SharedKey myacct:c2ln"), "")), Auth::SharedKey { account: "myacct".into(), lite: false });
        assert_eq!(classify(&req(Some("sharedkeylite acct2:c2ln=="), "")), Auth::SharedKey { account: "acct2".into(), lite: true });
    }

    #[test]
    fn bearer_and_anonymous() {
        assert_eq!(classify(&req(Some("Bearer eyJ0"), "")), Auth::Bearer);
        assert_eq!(classify(&req(Some("bearer x"), "")), Auth::Bearer);
        assert_eq!(classify(&req(None, "")), Auth::Anonymous);
        assert_eq!(classify(&req(None, "comp=list")), Auth::Anonymous);
        assert_eq!(classify(&req(Some("Basic abc"), "")), Auth::Anonymous);
        assert_eq!(classify(&req(Some(""), "")), Auth::Anonymous);
    }

    #[test]
    fn sas_parsing() {
        let q = "sv=2021-08-06&ss=bfqt&srt=sco&sp=rwdlacup&st=2022-10-27T00%3A00%3A00Z&se=2022-10-28T00%3A00%3A00Z&spr=https&sip=1.2.3.4&sr=b&si=pol&sig=ab%2Bc%3D&rscd=attachment&rsct=text%2Fplain&skoid=oid";
        match classify(&req(None, q)) {
            Auth::Sas(p) => {
                assert_eq!(p.sv.as_deref(), Some("2021-08-06"));
                assert_eq!(p.ss.as_deref(), Some("bfqt"));
                assert_eq!(p.srt.as_deref(), Some("sco"));
                assert_eq!(p.permissions(), "rwdlacup");
                assert_eq!(p.st.as_deref(), Some("2022-10-27T00:00:00Z"));
                assert_eq!(p.se.as_deref(), Some("2022-10-28T00:00:00Z"));
                assert_eq!(p.spr.as_deref(), Some("https"));
                assert_eq!(p.sip.as_deref(), Some("1.2.3.4"));
                assert_eq!(p.sr.as_deref(), Some("b"));
                assert_eq!(p.si.as_deref(), Some("pol"));
                assert_eq!(p.sig.as_deref(), Some("ab+c="));
                assert_eq!(p.rscd.as_deref(), Some("attachment"));
                assert_eq!(p.rsct.as_deref(), Some("text/plain"));
                assert_eq!(p.skoid.as_deref(), Some("oid"));
                assert_eq!(p.sktid, None);
            }
            other => panic!("expected Sas, got {other:?}"),
        }
    }

    #[test]
    fn authorization_header_beats_sas() {
        assert_eq!(classify(&req(Some("Bearer t"), "sig=x")), Auth::Bearer);
    }

    #[test]
    fn expiry() {
        let p = SasParams { se: Some("2022-10-28T00:00:00Z".into()), st: Some("2022-10-27T00:00:00Z".into()), ..Default::default() };
        assert!(!p.expired(datetime!(2022-10-27 12:00 UTC)));
        assert!(p.expired(datetime!(2022-10-28 00:00 UTC)));
        assert!(p.expired(datetime!(2023-01-01 00:00 UTC)));
        assert!(p.not_yet_valid(datetime!(2022-10-26 12:00 UTC)));
        assert!(!p.not_yet_valid(datetime!(2022-10-27 12:00 UTC)));
        assert!(!SasParams::default().expired(datetime!(2100-01-01 00:00 UTC)));
        assert_eq!(SasParams::default().permissions(), "");
        let date_only = SasParams { se: Some("2022-10-28".into()), ..Default::default() };
        assert!(date_only.expired(datetime!(2022-10-29 00:00 UTC)));
    }
}
