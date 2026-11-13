//! Path and method to handler.

use crate::account;
use crate::batch;
use crate::containers;
use crate::databases;
use crate::doc_query;
use crate::documents::{self, Loc};
use crate::errors::{CosmosError, R};
use crate::offers;
use crate::out::{Cx, Out};
use crate::patch;
use crate::pkranges;
use crate::query;
use crate::rid;
use crate::script_run::{self, DocOp};
use crate::scripts::{self, Kind};
use crate::state::{Account, State};
use crate::users;

fn not_built(cx: &Cx) -> CosmosError {
    if cx.req.segments().first().map(String::as_str) == Some("addresses") {
        // The address lookup of direct mode (`GET //addresses/`): a client in direct mode talks to the replicas over
        // TCP, which this emulator does not do. Say what to change.
        return CosmosError::not_implemented(&format!(
            "{} {} is the address lookup of direct mode, which this emulator does not have. Use gateway mode instead: \
             ConnectionMode.Gateway in the .NET SDK (and in the Azure Functions Cosmos DB extension: \"connectionMode\": \"Gateway\" in host.json), \
             gatewayMode() in the Java SDK. The Python, JavaScript and Go SDKs only use the gateway",
            cx.req.method,
            cx.req.decoded_path()
        ));
    }
    CosmosError::not_implemented(&format!("{} {}", cx.req.method, cx.req.decoded_path()))
}

pub fn dispatch(state: &mut State, cx: &Cx) -> R<Out> {
    let acct = state.accounts.entry(cx.req.account.clone()).or_default();
    let segs = cx.req.segments();
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let method = cx.req.method.as_str();
    match (method, s.as_slice()) {
        ("GET" | "HEAD", []) => Ok(account::read(cx, &cx.req.account, acct)),

        ("GET", ["offers"]) | ("POST", ["offers"]) => offers::list_or_query(cx, acct),
        ("GET", ["offers", id]) => offers::read(acct, id),
        ("PUT", ["offers", id]) => offers::replace(cx, acct, id),

        ("GET", ["dbs"]) => databases::list_or_query(cx, acct),
        ("POST", ["dbs"]) => {
            if query::is_query(cx) {
                databases::list_or_query(cx, acct)
            } else {
                databases::create(cx, acct)
            }
        }
        ("GET", ["dbs", d]) => databases::read(acct, d),
        ("DELETE", ["dbs", d]) => databases::delete(acct, d),

        (_, ["dbs", d, rest @ ..]) => {
            // The answer names the resource's owner twice, as the Java SDK reads it: the owner's name path
            // in `x-ms-alt-content-path` and its resource id in `x-ms-content-path`.
            let owner = owner_of(acct, d, method, rest);
            let out = db_scope(cx, acct, d, method, rest)?;
            Ok(match owner {
                Some((alt, rid)) => out.paths(alt, rid),
                None => out,
            })
        }
        _ => Err(not_built(cx)),
    }
}

/// Owner of what `rest` (a path below `dbs/{d}`) addresses: its name path and resource id. Looked up before
/// the handler runs, so a delete still finds it. Documents, scripts and partition key ranges belong to the
/// container; containers and users to the database; permissions to the user.
fn owner_of(acct: &Account, d: &str, method: &str, rest: &[&str]) -> Option<(String, String)> {
    let db = acct.db_name(d).and_then(|n| acct.databases.get(&n))?;
    let db_path = format!("dbs/{}", db.meta.id);
    match rest {
        // deleting a container: the SDK clears its session tokens by the id in this header, so it is the container's own
        ["colls", c] if method == "DELETE" => {
            let c = db.container_name(c).and_then(|n| db.containers.get(&n))?;
            Some((db_path, c.meta.rid.clone()))
        }
        ["colls", c, _, ..] => {
            let c = db.container_name(c).and_then(|n| db.containers.get(&n))?;
            Some((format!("{db_path}/colls/{}", c.meta.id), c.meta.rid.clone()))
        }
        ["users", u, "permissions", ..] => {
            let key = db
                .users
                .keys()
                .find(|k| *k == u)
                .cloned()
                .or_else(|| db.users.values().find(|r| rid::matches(&r.meta.rid, u)).map(|r| r.meta.id.clone()))?;
            let user = db.users.get(&key)?;
            Some((format!("{db_path}/users/{}", user.meta.id), user.meta.rid.clone()))
        }
        _ => Some((db_path, db.meta.rid.clone())),
    }
}

fn db_scope(cx: &Cx, acct: &mut Account, d: &str, method: &str, rest: &[&str]) -> R<Out> {
    match (method, rest) {
        ("GET", ["colls"]) => containers::list_or_query(cx, acct.db(d)?),
        ("POST", ["colls"]) => {
            if query::is_query(cx) {
                containers::list_or_query(cx, acct.db(d)?)
            } else {
                containers::create(cx, acct, d)
            }
        }
        ("GET", ["colls", c]) => containers::read(acct.db(d)?, c),
        ("PUT", ["colls", c]) => containers::replace(cx, acct.db(d)?, c),
        ("DELETE", ["colls", c]) => containers::delete(acct, d, c),

        ("GET", ["users"]) => users::list(cx, acct.db(d)?),
        ("POST", ["users"]) => {
            if query::is_query(cx) {
                users::list(cx, acct.db(d)?)
            } else {
                users::create(cx, acct.db(d)?, cx.flag("x-ms-documentdb-is-upsert"))
            }
        }
        ("GET", ["users", u]) => users::read(acct.db(d)?, u),
        ("PUT", ["users", u]) => users::replace(cx, acct.db(d)?, u),
        ("DELETE", ["users", u]) => users::delete(acct.db(d)?, u),
        ("GET", ["users", u, "permissions"]) => users::with_permissions(acct.db(d)?, u, |s| s.list(cx)),
        ("POST", ["users", u, "permissions"]) => {
            if query::is_query(cx) {
                users::with_permissions(acct.db(d)?, u, |s| s.list(cx))
            } else {
                let up = cx.flag("x-ms-documentdb-is-upsert");
                users::with_permissions(acct.db(d)?, u, |s| s.create(cx, up))
            }
        }
        ("GET", ["users", u, "permissions", p]) => users::with_permissions(acct.db(d)?, u, |s| s.read(p)),
        ("PUT", ["users", u, "permissions", p]) => users::with_permissions(acct.db(d)?, u, |s| s.replace(cx, p)),
        ("DELETE", ["users", u, "permissions", p]) => users::with_permissions(acct.db(d)?, u, |s| s.delete(p)),

        (_, ["colls", cname, tail @ ..]) => coll_scope(cx, acct, d, cname, method, tail),
        _ => Err(not_built(cx)),
    }
}

fn coll_scope(cx: &Cx, acct: &mut Account, d: &str, cname: &str, method: &str, rest: &[&str]) -> R<Out> {
    let db = acct.db(d)?;
    let loc = Loc { db_rid: db.meta.rid.clone() };
    let db_name = db.meta.id.clone();
    let c = db.container(cname)?;
    let db_rid = loc.db_rid.clone();
    match (method, rest) {
        ("GET", ["docs"]) => documents::list(cx, c),
        ("POST", ["docs"]) => {
            if cx.flag("x-ms-cosmos-is-batch-request") {
                batch::handle(cx, &loc, c)
            } else if cx.flag("x-ms-cosmos-is-query-plan-request") {
                doc_query::plan_request(cx)
            } else if query::is_query(cx) {
                doc_query::handle(cx, c)
            } else {
                let upsert = cx.flag("x-ms-documentdb-is-upsert");
                match script_run::prepare_doc_op(cx, &loc, &db_name, c, DocOp::Create { upsert }, None)? {
                    Some(job) => Ok(job),
                    None => documents::create(cx, &loc, c, upsert),
                }
            }
        }
        ("GET", ["docs", id]) => documents::read(cx, c, id),
        ("PUT", ["docs", id]) => match script_run::prepare_doc_op(cx, &loc, &db_name, c, DocOp::Replace, Some(id))? {
            Some(job) => Ok(job),
            None => documents::replace(cx, &loc, c, id),
        },
        ("DELETE", ["docs", id]) => match script_run::prepare_doc_op(cx, &loc, &db_name, c, DocOp::Delete, Some(id))? {
            Some(job) => Ok(job),
            None => documents::delete(cx, c, id),
        },
        ("PATCH", ["docs", id]) => patch::handle(cx, &loc, c, id),

        ("GET", ["pkranges"]) => {
            c.purge_expired(cx.now);
            pkranges::read(cx, &loc, c)
        }
        ("GET", ["conflicts"]) => query::feed(cx, &c.meta.rid, "Conflicts", Vec::new()),
        ("POST", ["operations", "partitionkeydelete"]) => documents::delete_partition(cx, c),

        ("GET", ["sprocs"]) => scripts::list(cx, c, &db_rid, Kind::Sproc),
        ("POST", ["sprocs"]) => {
            if query::is_query(cx) {
                scripts::list(cx, c, &db_rid, Kind::Sproc)
            } else {
                scripts::create(cx, c, &db_rid, Kind::Sproc)
            }
        }
        ("GET", ["sprocs", k]) => scripts::read(c, &db_rid, Kind::Sproc, k),
        ("PUT", ["sprocs", k]) => scripts::replace(cx, c, &db_rid, Kind::Sproc, k),
        ("DELETE", ["sprocs", k]) => scripts::delete(c, &db_rid, Kind::Sproc, k),
        ("POST", ["sprocs", k]) => scripts::execute(cx, c, &loc, &db_name, k),

        ("GET", ["triggers"]) => scripts::list(cx, c, &db_rid, Kind::Trigger),
        ("POST", ["triggers"]) => {
            if query::is_query(cx) {
                scripts::list(cx, c, &db_rid, Kind::Trigger)
            } else {
                scripts::create(cx, c, &db_rid, Kind::Trigger)
            }
        }
        ("GET", ["triggers", k]) => scripts::read(c, &db_rid, Kind::Trigger, k),
        ("PUT", ["triggers", k]) => scripts::replace(cx, c, &db_rid, Kind::Trigger, k),
        ("DELETE", ["triggers", k]) => scripts::delete(c, &db_rid, Kind::Trigger, k),

        ("GET", ["udfs"]) => scripts::list(cx, c, &db_rid, Kind::Udf),
        ("POST", ["udfs"]) => {
            if query::is_query(cx) {
                scripts::list(cx, c, &db_rid, Kind::Udf)
            } else {
                scripts::create(cx, c, &db_rid, Kind::Udf)
            }
        }
        ("GET", ["udfs", k]) => scripts::read(c, &db_rid, Kind::Udf, k),
        ("PUT", ["udfs", k]) => scripts::replace(cx, c, &db_rid, Kind::Udf, k),
        ("DELETE", ["udfs", k]) => scripts::delete(c, &db_rid, Kind::Udf, k),

        _ => Err(not_built(cx)),
    }
}
