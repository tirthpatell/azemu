//! Shared access signature strings to sign: account SAS, service SAS (Blob, Queue) and
//! user delegation SAS (Blob, Queue), for every signed version (`sv`) whose layout changed.
//!
//! Layouts (the Python SDK and Microsoft's Azurite agree on every one they implement;
//! the 2012-02-12 and 2013-08-15 service layouts come from the REST documentation):
//!
//! | what | `sv` from | change |
//! |---|---|---|
//! | service SAS | 2012-02-12 | `sp st se resource si sv` |
//! | | 2013-08-15 | response headers `rscc rscd rsce rscl rsct` |
//! | | 2015-02-21 | resource gets the service prefix (`/blob/acct/...`) |
//! | | 2015-04-05 | `sip spr` after `si` |
//! | | 2018-11-09 | `sr` and the snapshot / version time after `sv` |
//! | | 2020-12-06 | `ses` after the snapshot time |
//! | account SAS | 2015-04-05 | `account sp ss srt st se sip spr sv` + empty line |
//! | | 2020-12-06 | `ses` after `sv` |
//! | user delegation | 2018-11-09 | `sp st se resource skoid sktid skt ske sks skv sip spr sv sr time rsc*` |
//! | | 2020-02-10 | `saoid suoid scid` after `skv` (Blob) |
//! | | 2020-12-06 | `ses` after the time |
//! | | 2025-07-05 | `skdutid sduoid` (after `scid` for Blob, after `skv` for Queue) |
//! | | 2026-04-06 | canonicalized `srh` and `srq` after `ses` (Blob) |
//!
//! No layout changed between 2026-04-06 and 2026-12-06 (the Go SDK, `azblob` 1.8.3 `sas/`, signs with
//! `sv=2026-12-06` and writes the same strings as the JavaScript SDK does for 2026-04-06 and later).

use crate::{Query, Request};

use super::StorageService;

/// Newest `sv` whose SAS layout is known: 2026-12-06 is what the Go SDK signs with, and its
/// layout (and the JavaScript SDK's for 2026-10-06) is the one of 2026-04-06. A SAS signed with a
/// newer `sv` is checked with that newest layout; when it does not match it is refused, with a
/// message that says the version is newer than the layouts azemu knows.
pub const NEWEST_KNOWN_VERSION: &str = "2026-12-06";

/// `YYYY-MM-DD`.
pub fn is_version(sv: &str) -> bool {
    let b = sv.as_bytes();
    b.len() == 10 && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// Every SAS query field, as sent (percent-decoded), "" when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SasFields {
    pub sv: String,
    pub sp: String,
    pub st: String,
    pub se: String,
    pub sip: String,
    pub spr: String,
    pub si: String,
    pub sr: String,
    pub sig: String,
    pub ss: String,
    pub srt: String,
    pub skoid: String,
    pub sktid: String,
    pub skt: String,
    pub ske: String,
    pub sks: String,
    pub skv: String,
    pub saoid: String,
    pub suoid: String,
    pub scid: String,
    pub skdutid: String,
    pub sduoid: String,
    pub ses: String,
    pub srh: String,
    pub srq: String,
    pub rscc: String,
    pub rscd: String,
    pub rsce: String,
    pub rscl: String,
    pub rsct: String,
}

impl SasFields {
    pub fn from_query(q: &Query) -> SasFields {
        let g = |n: &str| q.get(n).unwrap_or("").to_string();
        SasFields {
            sv: g("sv"),
            sp: g("sp"),
            st: g("st"),
            se: g("se"),
            sip: g("sip"),
            spr: g("spr"),
            si: g("si"),
            sr: g("sr"),
            sig: g("sig"),
            ss: g("ss"),
            srt: g("srt"),
            skoid: g("skoid"),
            sktid: g("sktid"),
            skt: g("skt"),
            ske: g("ske"),
            sks: g("sks"),
            skv: g("skv"),
            saoid: g("saoid"),
            suoid: g("suoid"),
            scid: g("scid"),
            skdutid: g("skdutid"),
            sduoid: g("sduoid"),
            ses: g("ses"),
            srh: g("srh"),
            srq: g("srq"),
            rscc: g("rscc"),
            rscd: g("rscd"),
            rsce: g("rsce"),
            rscl: g("rscl"),
            rsct: g("rsct"),
        }
    }

    /// An account SAS names services and resource types.
    pub fn is_account(&self) -> bool {
        !self.ss.is_empty() || !self.srt.is_empty()
    }

    pub fn is_user_delegation(&self) -> bool {
        !self.skoid.is_empty()
    }

    /// The version used to pick a layout: `sv`, or the newest known when `sv` is not a date.
    pub fn layout_version(&self) -> &str {
        if is_version(&self.sv) {
            &self.sv
        } else {
            NEWEST_KNOWN_VERSION
        }
    }
}

/// What a service SAS was issued for, as the request names it (percent-decoded).
#[derive(Debug, Clone, Copy)]
pub enum SasTarget<'a> {
    Blob { container: &'a str, blob: Option<&'a str> },
    Queue { queue: Option<&'a str> },
}

/// Account SAS string to sign.
pub fn account_sas_string_to_sign(account: &str, f: &SasFields) -> String {
    let ver = f.layout_version();
    let mut s = format!("{account}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n", f.sp, f.ss, f.srt, f.st, f.se, f.sip, f.spr, f.sv);
    if ver >= "2020-12-06" {
        s.push_str(&f.ses);
        s.push('\n');
    }
    s
}

/// Canonicalized resource of a service SAS: `/blob/acct/container/blob`, `/queue/acct/queue`;
/// before 2015-02-21 without the service name. `path` has no leading slash.
pub fn canonical_resource(service: StorageService, account: &str, path: &str, ver: &str, with_prefix: Option<bool>) -> String {
    let prefix = with_prefix.unwrap_or(ver >= "2015-02-21");
    if prefix {
        format!("/{}/{account}/{path}", service.name())
    } else {
        format!("/{account}/{path}")
    }
}

/// Response-header overrides in string order.
fn rsc(f: &SasFields) -> [&str; 5] {
    [&f.rscc, &f.rscd, &f.rsce, &f.rscl, &f.rsct]
}

/// Service SAS (shared key) string to sign. `path` is the resource without a leading
/// slash (`container`, `container/blob`, `queue`). `time` is the snapshot or version
/// time of a `bs` / `bv` SAS. `prefix` overrides whether the service name is in the
/// canonicalized resource.
pub fn service_sas_string_to_sign(
    service: StorageService,
    account: &str,
    f: &SasFields,
    path: &str,
    time: &str,
    prefix: Option<bool>,
) -> String {
    let ver = f.layout_version();
    let canon = canonical_resource(service, account, path, ver, prefix);
    let mut v: Vec<&str> = vec![&f.sp, &f.st, &f.se, &canon, &f.si];
    if ver >= "2015-04-05" {
        v.push(&f.sip);
        v.push(&f.spr);
    }
    v.push(&f.sv);
    if service == StorageService::Blob {
        if ver >= "2018-11-09" {
            v.push(&f.sr);
            v.push(time);
        }
        if ver >= "2020-12-06" {
            v.push(&f.ses);
        }
        if ver >= "2013-08-15" {
            v.extend(rsc(f));
        }
    }
    v.join("\n")
}

/// Canonicalized signed request headers and query parameters of a 2026-04-06 user
/// delegation SAS. `srq_sep` is the character between name and value (`:` as the Python
/// SDK signs it, `=` as the REST documentation says).
fn srh_srq(req: &Request, f: &SasFields, srq_sep: char) -> (String, String) {
    let mut h = String::new();
    if !f.srh.is_empty() {
        for k in f.srh.split(',') {
            let vals: Vec<&str> = req.headers.get_all(k.trim()).iter().filter_map(|v| v.to_str().ok()).map(str::trim).collect();
            h.push_str(&format!("{k}:{}\n", vals.join(",")));
        }
    }
    let mut q = String::new();
    if !f.srq.is_empty() {
        for k in f.srq.split(',') {
            let vals: Vec<&str> = req.query.get_all(k).collect();
            q.push_str(&format!("\n{k}{srq_sep}{}", vals.join(",")));
        }
    }
    (h, q)
}

/// User delegation SAS strings to sign: one per reading of the layout. More than one
/// only where the sources disagree (`sv` before 2020-02-10, the `srq` separator); the
/// bool is true when the layout for this `sv` is not certain.
pub fn user_delegation_sas_strings(
    service: StorageService,
    account: &str,
    f: &SasFields,
    path: &str,
    time: &str,
    req: &Request,
) -> (Vec<String>, bool) {
    let ver = f.layout_version();
    let canon = canonical_resource(service, account, path, ver, None);
    let blob = service == StorageService::Blob;
    let mut out: Vec<String> = Vec::new();
    let mut uncertain = false;

    // (with saoid group, with snapshot-time slot) readings for the layout
    let mut shapes: Vec<(bool, bool)> = vec![(blob && ver >= "2020-02-10", true)];
    if blob && ver < "2020-02-10" {
        // Azurite: no saoid group, time slot; REST docs: saoid group, no time slot.
        shapes = vec![(false, true), (true, false), (true, true)];
        uncertain = true;
    }
    let seps: &[char] = if blob && ver >= "2026-04-06" && !f.srq.is_empty() { &[':', '='] } else { &[':'] };
    for (saoid_group, time_slot) in shapes {
        for sep in seps {
            let mut v: Vec<String> = vec![f.sp.clone(), f.st.clone(), f.se.clone(), canon.clone()];
            v.extend([&f.skoid, &f.sktid, &f.skt, &f.ske, &f.sks, &f.skv].map(|s| s.clone()));
            if saoid_group {
                v.extend([&f.saoid, &f.suoid, &f.scid].map(|s| s.clone()));
            }
            if ver >= "2025-07-05" {
                v.push(f.skdutid.clone());
                v.push(f.sduoid.clone());
            }
            v.extend([&f.sip, &f.spr, &f.sv].map(|s| s.clone()));
            if blob {
                v.push(f.sr.clone());
                if time_slot {
                    v.push(time.to_string());
                }
                if ver >= "2020-12-06" {
                    v.push(f.ses.clone());
                }
                if ver >= "2026-04-06" {
                    let (h, q) = srh_srq(req, f, *sep);
                    v.push(h);
                    v.push(q);
                }
                v.extend(rsc(f).map(|s| s.to_string()));
            }
            let s = v.join("\n");
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    (out, uncertain)
}
