//! Data Lake request routing: split the URL, identify the operation, authorize, dispatch.

use azemu_core::http::Method;
use azemu_core::resp::Resp;
use azemu_core::storage::{self, StorageAccounts};
use azemu_core::{Request, Response};

use crate::cx::Cx;
use crate::dfs::auth::{self, Target};
use crate::dfs::errors::{into_response, not_implemented};
use crate::dfs::model::{live, norm};
use crate::dfs::ops::{identify, Op};
use crate::dfs::{access, deleted, expiry, filesystem, list, path, rename, update};
use crate::errors::{ApiError, ApiResult};
use crate::events;
use crate::pre::Pre;
use crate::router::{check_version, decode};
use crate::state::State;

pub const SERVER: &str = "Windows-Azure-HDFS/1.0 Microsoft-HTTPAPI/2.0";

pub fn run(st: &mut State, req: &Request, accounts: &StorageAccounts, prep: &Pre) -> Response {
    // CORS is the account's (the Blob service properties), judged by the code Blob, Queue and Table share.
    let rules = crate::service_props::cors_rules(&st.account(&req.account.to_ascii_lowercase()).props);
    if req.method == Method::OPTIONS {
        return storage::finish(storage::cors::preflight(req, &rules), req, SERVER);
    }
    let mut resp = match run_inner(st, req, accounts, prep) {
        Ok(r) => r,
        Err(e) => into_response(e, &req.ctx),
    };
    if req.header("origin").is_some() {
        storage::cors::apply(&mut resp, req, &rules);
    }
    storage::finish(resp, req, SERVER)
}

/// `(filesystem, normalized path, the URL names a path)` from the URL path. `/fs//` names the
/// root directory (path empty, named); `/fs` and `/fs/` name the filesystem only.
fn parse_target(req: &Request) -> (Option<String>, String, bool) {
    let p = req.path.trim_start_matches('/');
    if p.is_empty() {
        return (None, String::new(), false);
    }
    let (fs, rest) = p.split_once('/').unwrap_or((p, ""));
    let named = !rest.is_empty() || req.query.has("action");
    (Some(decode(fs)), norm(&decode(rest)), named)
}

/// The operation a request asks for (before any authorization).
pub(crate) fn op_of(req: &Request) -> Op {
    let (fs, _, named) = parse_target(req);
    identify(req, fs.is_some(), named)
}

fn run_inner(st: &mut State, req: &Request, accounts: &StorageAccounts, prep: &Pre) -> ApiResult {
    let (fs, path, named) = parse_target(req);
    let op = identify(req, fs.is_some(), named);
    check_version(req)?;
    let acct_name = req.account.to_ascii_lowercase();

    let authz = {
        let acct = st.account(&acct_name);
        let c = fs.as_deref().and_then(|n| acct.containers.get(n));
        let path_exists = c.map(|c| path.is_empty() || live(c, &path).is_some()).unwrap_or(false);
        let t = Target {
            container: c,
            public_access_allowed: acct.public_access_allowed,
            path_exists,
            container_name: fs.as_deref(),
            path: if named { Some(path.as_str()) } else { None },
            accounts,
        };
        auth::authorize(req, op, &t)?
    };

    let pre = if st.events_wanted { events::before_dfs(st, &acct_name, op, fs.as_deref(), &path) } else { None };
    let mut cx = Cx { req, st, now: req.ctx.time, acct: acct_name, authz, pre: prep };
    let result = dispatch(&mut cx, op, fs.as_deref().unwrap_or(""), &path);
    if let Ok(resp) = &result {
        if cx.st.events_wanted {
            let acct = cx.acct.clone();
            events::after_dfs(cx.st, req, &acct, op, fs.as_deref(), &path, pre, resp);
        }
    }
    result
}

fn dispatch(cx: &mut Cx, op: Op, fs: &str, p: &str) -> ApiResult {
    match op {
        Op::AccountInfo => Ok(Resp::new(200u16)
            .header("x-ms-sku-name", "Standard_RAGRS")
            .header("x-ms-account-kind", "StorageV2")
            .header("x-ms-is-hns-enabled", "true")
            .build()),
        Op::ListFileSystems => list::list_filesystems(cx),
        Op::CreateFileSystem => filesystem::create(cx, fs),
        Op::SetFileSystemProperties => filesystem::set_properties(cx, fs),
        Op::GetFileSystemProperties => filesystem::get_properties(cx, fs),
        Op::DeleteFileSystem => filesystem::delete(cx, fs),
        Op::ListPaths => list::list_paths(cx, fs),
        Op::ListBlobs => {
            filesystem::open(cx, fs)?;
            crate::list::list_blobs(cx, fs)
        }
        Op::CreatePath { dir } => path::create(cx, fs, p, dir),
        Op::RenamePath => rename::rename(cx, fs, p),
        Op::ReadPath => path::read(cx, fs, p),
        Op::GetPathProperties => path::get_properties(cx, fs, p),
        Op::DeletePath => path::delete(cx, fs, p),
        Op::Append => update::append(cx, fs, p),
        Op::Flush => update::flush(cx, fs, p),
        Op::SetProperties => update::set_properties(cx, fs, p),
        Op::SetAccessControl => access::set_access_control(cx, fs, p),
        Op::SetAccessControlRecursive => access::set_access_control_recursive(cx, fs, p),
        Op::LeasePath => path::lease(cx, fs, p),
        Op::SetExpiry => {
            filesystem::open(cx, fs)?;
            expiry::set_expiry(cx, fs, p)
        }
        Op::Undelete => {
            filesystem::open(cx, fs)?;
            deleted::undelete(cx, fs, p)
        }
        Op::Query => {
            filesystem::open(cx, fs)?;
            crate::query::query_blob(cx, fs, p, true)
        }
        Op::Unknown => Err(unknown(cx)),
    }
}

fn unknown(cx: &Cx) -> ApiError {
    let req = cx.req;
    let m = req.method.as_str();
    if !matches!(m, "GET" | "HEAD" | "PUT" | "POST" | "PATCH" | "DELETE") {
        return crate::errors::err("UnsupportedHttpVerb");
    }
    let err = not_implemented(&format!("{m} {} ?{}", req.decoded_path(), req.query.raw()));
    // the name for `/_azemu/unsupported` carries the shape of the request, not the account's own names
    let shape =
        ["resource", "action", "comp"].iter().filter_map(|k| req.query.get(k).map(|v| format!("{k}={v}"))).collect::<Vec<_>>().join("&");
    err.unsupported(format!("DataLake_Unknown ({m} {shape})"))
}
