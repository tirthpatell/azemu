//! Turn method, path and query into one operation.

use azemu_core::http::Method;
use azemu_core::Request;

use crate::names;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // service
    GetServiceProperties,
    SetServiceProperties,
    ListShares,
    GetUserDelegationKey,
    // shares
    CreateShare,
    GetShareProperties,
    DeleteShare,
    SetShareProperties,
    SetShareMetadata,
    GetShareMetadata,
    GetShareAcl,
    SetShareAcl,
    GetShareStats,
    CreateShareSnapshot,
    LeaseShare,
    RestoreShare,
    CreatePermission,
    GetPermission,
    // directories (the root of a share is one, with an empty path)
    CreateDirectory,
    GetDirectoryProperties,
    DeleteDirectory,
    SetDirectoryProperties,
    SetDirectoryMetadata,
    GetDirectoryMetadata,
    ListFilesAndDirectories,
    RenameDirectory,
    // either of them
    ListHandles,
    ForceCloseHandles,
    // files
    CreateFile,
    GetFile,
    DeleteFile,
    SetFileProperties,
    SetFileMetadata,
    GetFileMetadata,
    LeaseFile,
    PutRange,
    PutRangeFromUrl,
    ListRanges,
    CopyFile,
    AbortCopy,
    RenameFile,
    /// An operation this emulator does not build: answers 501 with this name.
    NotBuilt(&'static str),
}

/// An operation and what it is aimed at.
#[derive(Debug, Clone)]
pub struct Routed {
    pub op: Op,
    pub share: Option<String>,
    /// The names inside the share (empty for the share itself and its root directory).
    pub path: Vec<String>,
}

/// Why a request did not become an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrouted {
    UnsupportedVerb,
    /// 400 `InvalidUri` (names the path).
    BadUri(String),
    /// 400 `InvalidQueryParameterValue`.
    BadQuery(&'static str, String),
}

pub fn route(req: &Request) -> Result<Routed, Unrouted> {
    let segs = req.segments();
    let m = &req.method;
    let comp = req.query.get("comp");
    let restype = req.query.get("restype");
    let get = *m == Method::GET || *m == Method::HEAD;
    let put = *m == Method::PUT;

    let bad_uri = || Unrouted::BadUri(req.path.clone());
    let ok = |op: Op, share: Option<&str>, path: Vec<String>| Ok(Routed { op, share: share.map(str::to_string), path });
    let verb = || Err(Unrouted::UnsupportedVerb);

    let Some(share) = segs.first() else {
        // the account
        return match (restype, comp) {
            (None, Some("list")) | (Some(""), Some("list")) => {
                if get {
                    ok(Op::ListShares, None, vec![])
                } else {
                    verb()
                }
            }
            (Some("service"), Some("properties")) => {
                if get {
                    ok(Op::GetServiceProperties, None, vec![])
                } else if put {
                    ok(Op::SetServiceProperties, None, vec![])
                } else {
                    verb()
                }
            }
            (Some("service"), Some("userdelegationkey")) => {
                if *m == Method::POST {
                    ok(Op::GetUserDelegationKey, None, vec![])
                } else {
                    verb()
                }
            }
            (_, Some(c)) => Err(Unrouted::BadQuery("comp", c.to_string())),
            _ => Err(bad_uri()),
        };
    };

    // NFS: files by their id, and hard links
    if req.query.has("fileid") || comp == Some("hardlinks") {
        return Ok(Routed { op: Op::NotBuilt("NfsFileById"), share: Some(share.clone()), path: vec![] });
    }
    let names: Vec<String> = names::split_path(&segs[1..].join("/"), names::allow_trailing_dot(req));
    let share = Some(share.as_str());

    // The share itself and its root directory.
    if names.is_empty() {
        return match restype {
            Some("share") => share_op(comp, share, m),
            Some("directory") => dir_op(comp, share, names, m),
            None if matches!(comp, Some("listhandles" | "forceclosehandles")) => handles(comp, share, names, m),
            _ => Err(bad_uri()),
        };
    }

    match restype {
        Some("directory") => dir_op(comp, share, names, m),
        None | Some("") => file_op(req, comp, share, names, m),
        Some("symboliclink") => ok(Op::NotBuilt(if get { "GetSymbolicLink" } else { "CreateSymbolicLink" }), share, names),
        Some("hardlink") => ok(Op::NotBuilt("CreateHardLink"), share, names),
        Some(other) => Err(Unrouted::BadQuery("restype", other.to_string())),
    }
}

fn share_op(comp: Option<&str>, share: Option<&str>, m: &Method) -> Result<Routed, Unrouted> {
    let get = *m == Method::GET || *m == Method::HEAD;
    let put = *m == Method::PUT;
    let op = match comp {
        None => match *m {
            Method::PUT => Op::CreateShare,
            Method::DELETE => Op::DeleteShare,
            _ if get => Op::GetShareProperties,
            _ => return Err(Unrouted::UnsupportedVerb),
        },
        Some("properties") if put => Op::SetShareProperties,
        Some("metadata") if put => Op::SetShareMetadata,
        Some("metadata") if get => Op::GetShareMetadata,
        Some("acl") if put => Op::SetShareAcl,
        Some("acl") if get => Op::GetShareAcl,
        Some("stats") if get => Op::GetShareStats,
        Some("snapshot") if put => Op::CreateShareSnapshot,
        Some("lease") if put => Op::LeaseShare,
        Some("undelete") if put => Op::RestoreShare,
        Some("filepermission") if put => Op::CreatePermission,
        Some("filepermission") if get => Op::GetPermission,
        Some("properties" | "metadata" | "acl" | "stats" | "snapshot" | "lease" | "undelete" | "filepermission") => {
            return Err(Unrouted::UnsupportedVerb)
        }
        Some(c) => return Err(Unrouted::BadQuery("comp", c.to_string())),
    };
    Ok(Routed { op, share: share.map(str::to_string), path: vec![] })
}

fn handles(comp: Option<&str>, share: Option<&str>, path: Vec<String>, m: &Method) -> Result<Routed, Unrouted> {
    let op = match comp {
        Some("listhandles") if *m == Method::GET => Op::ListHandles,
        Some("forceclosehandles") if *m == Method::PUT => Op::ForceCloseHandles,
        _ => return Err(Unrouted::UnsupportedVerb),
    };
    Ok(Routed { op, share: share.map(str::to_string), path })
}

fn dir_op(comp: Option<&str>, share: Option<&str>, path: Vec<String>, m: &Method) -> Result<Routed, Unrouted> {
    let get = *m == Method::GET || *m == Method::HEAD;
    let put = *m == Method::PUT;
    let op = match comp {
        None => match *m {
            Method::PUT => Op::CreateDirectory,
            Method::DELETE => Op::DeleteDirectory,
            _ if get => Op::GetDirectoryProperties,
            _ => return Err(Unrouted::UnsupportedVerb),
        },
        Some("properties") if put => Op::SetDirectoryProperties,
        Some("metadata") if put => Op::SetDirectoryMetadata,
        Some("metadata") if get => Op::GetDirectoryMetadata,
        Some("list") if *m == Method::GET => Op::ListFilesAndDirectories,
        Some("rename") if put => Op::RenameDirectory,
        Some("listhandles" | "forceclosehandles") => return handles(comp, share, path, m),
        Some("properties" | "metadata" | "list" | "rename") => return Err(Unrouted::UnsupportedVerb),
        // Azure names `restype` here, with the value of `comp`
        Some(c) => return Err(Unrouted::BadQuery("restype", c.to_string())),
    };
    Ok(Routed { op, share: share.map(str::to_string), path })
}

fn file_op(req: &Request, comp: Option<&str>, share: Option<&str>, path: Vec<String>, m: &Method) -> Result<Routed, Unrouted> {
    let get = *m == Method::GET || *m == Method::HEAD;
    let put = *m == Method::PUT;
    let op = match comp {
        None => match *m {
            // Copy File is a Put File with a copy source (the swagger names it `comp=copy`, which no client sends)
            Method::PUT if req.header("x-ms-copy-source").is_some() => Op::CopyFile,
            Method::PUT => Op::CreateFile,
            Method::DELETE => Op::DeleteFile,
            _ if get => Op::GetFile,
            _ => return Err(Unrouted::UnsupportedVerb),
        },
        Some("properties") if put => Op::SetFileProperties,
        Some("metadata") if put => Op::SetFileMetadata,
        Some("metadata") if get => Op::GetFileMetadata,
        Some("lease") if put => Op::LeaseFile,
        Some("range") if put => {
            if req.header("x-ms-copy-source").is_some() {
                Op::PutRangeFromUrl
            } else {
                Op::PutRange
            }
        }
        Some("rangelist") if *m == Method::GET => Op::ListRanges,
        Some("copy") if put => {
            if req.query.has("copyid") {
                Op::AbortCopy
            } else {
                Op::CopyFile
            }
        }
        Some("rename") if put => Op::RenameFile,
        Some("listhandles" | "forceclosehandles") => return handles(comp, share, path, m),
        Some("properties" | "metadata" | "lease" | "range" | "rangelist" | "copy" | "rename") => return Err(Unrouted::UnsupportedVerb),
        // Azure names `restype` here, with the value of `comp`
        Some(c) => return Err(Unrouted::BadQuery("restype", c.to_string())),
    };
    Ok(Routed { op, share: share.map(str::to_string), path })
}
