//! Pieces shared by the operation handlers.

use azemu_core::resp::Resp;
use azemu_core::Request;

use crate::errors::{table_not_found, Res};
use crate::odata::{Fmt, Level};
use crate::routing::table_name_error;
use crate::sas::Grant;
use crate::state::{Account, Table};
use crate::values::Ticks;

/// What a handler knows about the request it is serving.
pub struct Rq<'a> {
    pub req: &'a Request,
    pub base: String,
    pub level: Level,
    pub grant: Grant,
    pub now: Ticks,
}

impl<'a> Rq<'a> {
    pub fn new(req: &'a Request, grant: Grant) -> Rq<'a> {
        Rq {
            req,
            base: req.ctx.endpoint.base_url(),
            level: Level::from_accept(req.header("accept")),
            grant,
            now: crate::values::ticks_from_time(req.ctx.time),
        }
    }

    pub fn fmt<'b>(&'b self, table: &'b str) -> Fmt<'b> {
        Fmt { level: self.level, base: &self.base, account: &self.req.account, table, select_raw: self.query("$select") }
    }

    /// A JSON response in the negotiated payload format.
    pub fn json(&self, status: u16, body: String) -> Resp {
        base_headers(Resp::new(status)).header("Content-Type", self.level.content_type()).body(body)
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.req.query.get(name)
    }
}

/// Headers every JSON-side response carries.
pub fn base_headers(r: Resp) -> Resp {
    r.header("Cache-Control", "no-cache").header("X-Content-Type-Options", "nosniff")
}

/// Look a table up case-insensitively.
pub fn table<'a>(acct: &'a mut Account, name: &str) -> Res<&'a mut Table> {
    acct.tables.get_mut(&name.to_ascii_lowercase()).ok_or_else(table_not_found)
}

pub fn table_ref<'a>(acct: &'a Account, name: &str) -> Res<&'a Table> {
    acct.tables.get(&name.to_ascii_lowercase()).ok_or_else(table_not_found)
}

pub fn check_name(name: &str) -> Res<()> {
    match table_name_error(name) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Continuation tokens are `1!<len>!<base64 with = written as ->`.
pub fn encode_token(s: &str) -> String {
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(s.as_bytes()).replace('=', "-");
    format!("1!{}!{}", b.len(), b)
}

pub fn decode_token(t: &str) -> Option<String> {
    use base64::Engine;
    let mut it = t.splitn(3, '!');
    let (v, len, b) = (it.next()?, it.next()?, it.next()?);
    if v != "1" || len.parse::<usize>().ok()? != b.len() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(b.replace('-', "=")).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_recorded_form() {
        assert_eq!(encode_token("pk"), "1!4!cGs-");
        assert_eq!(encode_token("1012"), "1!8!MTAxMg--");
        assert_eq!(decode_token("1!4!cGs-").as_deref(), Some("pk"));
        assert_eq!(decode_token("1!8!MTAxMjM0").as_deref(), Some("101234"));
        assert_eq!(decode_token("garbage"), None);
        assert_eq!(decode_token("1!9!cGs-"), None);
        assert_eq!(decode_token(&encode_token("héllo 你好")).as_deref(), Some("héllo 你好"));
    }
}
