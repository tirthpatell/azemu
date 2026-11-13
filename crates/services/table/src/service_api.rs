//! Service properties, statistics and stored access policies (all XML).

use azemu_core::resp::Resp;
use azemu_core::{time, Response};

use crate::acl;
use crate::api::{table, table_ref, Rq};
use crate::errors::Res;
use crate::props;
use crate::sas::Op;
use crate::state::Account;

pub fn get_properties(rq: &Rq, acct: &Account) -> Res<Response> {
    rq.grant.check(Op::ServiceGet, None)?;
    Ok(Resp::new(200u16).xml(props::render(&acct.props)).build())
}

pub fn set_properties(rq: &Rq, acct: &mut Account) -> Res<Response> {
    rq.grant.check(Op::ServiceSet, None)?;
    let body = String::from_utf8_lossy(&rq.req.body).into_owned();
    acct.props = props::apply(&acct.props, &body)?;
    Ok(Resp::new(202u16).build())
}

pub fn stats(rq: &Rq) -> Res<Response> {
    rq.grant.check(Op::ServiceGet, None)?;
    Ok(Resp::new(200u16).xml(props::stats_xml(&time::rfc1123(rq.req.ctx.time))).build())
}

pub fn get_acl(rq: &Rq, acct: &Account, name: &str) -> Res<Response> {
    rq.grant.check(Op::AclGet, None)?;
    let t = table_ref(acct, name)?;
    Ok(Resp::new(200u16).xml(acl::render(&t.acl)).build())
}

pub fn set_acl(rq: &Rq, acct: &mut Account, name: &str) -> Res<Response> {
    rq.grant.check(Op::AclSet, None)?;
    let t = table(acct, name)?;
    let body = String::from_utf8_lossy(&rq.req.body).into_owned();
    // an empty body clears the policies
    t.acl = if body.trim().is_empty() { Vec::new() } else { acl::parse(&body)? };
    Ok(Resp::new(204u16).build())
}
