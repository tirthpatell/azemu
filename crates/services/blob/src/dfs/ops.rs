//! Which Data Lake operation a request is, and what it needs.

use azemu_core::Request;

use crate::ops::{Anon, Level};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    AccountInfo,
    ListFileSystems,
    CreateFileSystem,
    SetFileSystemProperties,
    GetFileSystemProperties,
    DeleteFileSystem,
    ListPaths,
    ListBlobs,
    CreatePath {
        dir: bool,
    },
    RenamePath,
    ReadPath,
    GetPathProperties,
    DeletePath,
    Append,
    Flush,
    SetProperties,
    SetAccessControl,
    SetAccessControlRecursive,
    LeasePath,
    SetExpiry,
    Undelete,
    /// Query Blob Contents (`POST ?comp=query`) on a file.
    Query,
    Unknown,
}

pub struct Need {
    pub level: Level,
    pub perms: &'static str,
    pub perms_new: Option<&'static str>,
    pub anon: Anon,
}

const fn need(level: Level, perms: &'static str, anon: Anon) -> Need {
    Need { level, perms, perms_new: None, anon }
}

impl Op {
    pub fn need(self) -> Need {
        match self {
            Op::AccountInfo => need(Level::Any, "", Anon::No),
            Op::ListFileSystems => need(Level::Service, "l", Anon::No),
            Op::CreateFileSystem => need(Level::Container, "cw", Anon::No),
            Op::DeleteFileSystem => need(Level::Container, "d", Anon::No),
            Op::GetFileSystemProperties => need(Level::Container, "r", Anon::Container),
            Op::SetFileSystemProperties => need(Level::Container, "w", Anon::No),
            Op::ListPaths | Op::ListBlobs => need(Level::Container, "l", Anon::Container),
            Op::CreatePath { .. } => Need { level: Level::Object, perms: "w", perms_new: Some("cw"), anon: Anon::No },
            Op::RenamePath => need(Level::Object, "mcw", Anon::No),
            Op::ReadPath | Op::GetPathProperties => need(Level::Object, "re", Anon::Blob),
            Op::DeletePath => need(Level::Object, "d", Anon::No),
            Op::Append => need(Level::Object, "aw", Anon::No),
            Op::Flush | Op::SetProperties | Op::LeasePath | Op::SetExpiry => need(Level::Object, "w", Anon::No),
            Op::SetAccessControl | Op::SetAccessControlRecursive => need(Level::Object, "pow", Anon::No),
            Op::Undelete => need(Level::Object, "wd", Anon::No),
            Op::Query => need(Level::Object, "re", Anon::No),
            Op::Unknown => need(Level::Service, "", Anon::No),
        }
    }
}

/// `has_fs`: the URL names a filesystem; `has_path`: it names a path (the root directory
/// counts: `/fs//`).
pub fn identify(req: &Request, has_fs: bool, has_path: bool) -> Op {
    let m = req.method.as_str();
    let q = &req.query;
    let lower = |n: &str| q.get(n).map(|s| s.to_ascii_lowercase());
    let resource = lower("resource");
    let action = lower("action");
    let comp = lower("comp");
    let restype = lower("restype");
    let (resource, action, comp, restype) = (resource.as_deref(), action.as_deref(), comp.as_deref(), restype.as_deref());

    if restype == Some("account") && comp == Some("properties") {
        return Op::AccountInfo;
    }
    if !has_fs {
        return match (m, resource) {
            ("GET", Some("account")) => Op::ListFileSystems,
            _ => Op::Unknown,
        };
    }
    if !has_path {
        return match (m, resource, comp) {
            ("PUT", _, None) => Op::CreateFileSystem,
            ("PATCH", _, None) => Op::SetFileSystemProperties,
            ("HEAD", _, None) => Op::GetFileSystemProperties,
            ("DELETE", _, None) => Op::DeleteFileSystem,
            ("GET", Some("filesystem"), None) => Op::ListPaths,
            ("GET", _, Some("list")) if restype == Some("container") => Op::ListBlobs,
            _ => Op::Unknown,
        };
    }
    match m {
        "PUT" => {
            if req.header("x-ms-rename-source").is_some() {
                return Op::RenamePath;
            }
            match (resource, comp) {
                (Some("directory"), None) => Op::CreatePath { dir: true },
                (Some("file"), None) => Op::CreatePath { dir: false },
                (_, Some("expiry")) => Op::SetExpiry,
                (_, Some("undelete")) => Op::Undelete,
                _ => Op::Unknown,
            }
        }
        "PATCH" => match action {
            Some("append") => Op::Append,
            Some("flush") => Op::Flush,
            Some("setproperties") => Op::SetProperties,
            Some("setaccesscontrol") => Op::SetAccessControl,
            Some("setaccesscontrolrecursive") => Op::SetAccessControlRecursive,
            _ => Op::Unknown,
        },
        "POST" if comp == Some("query") => Op::Query,
        "POST" => Op::LeasePath,
        "GET" => Op::ReadPath,
        "HEAD" => Op::GetPathProperties,
        "DELETE" => Op::DeletePath,
        _ => Op::Unknown,
    }
}
