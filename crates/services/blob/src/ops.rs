//! Which operation a request is, and what it needs to be allowed (SAS permission
//! letters, resource type, anonymous access).

use azemu_core::Request;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    // service
    ListContainers,
    GetServiceProperties,
    SetServiceProperties,
    GetServiceStats,
    GetUserDelegationKey,
    GetAccountInfo,
    FindBlobsAccount,
    BatchAccount,
    // container
    CreateContainer,
    DeleteContainer,
    GetContainerProperties,
    GetContainerMetadata,
    SetContainerMetadata,
    GetContainerAcl,
    SetContainerAcl,
    LeaseContainer,
    ListBlobs,
    FindBlobsContainer,
    BatchContainer,
    UndeleteContainer,
    RenameContainer,
    CreateSession,
    // blob
    PutBlob,
    PutBlobFromUrl,
    CopyBlob,
    AbortCopy,
    IncrementalCopy,
    GetBlob,
    GetBlobProperties,
    GetBlobMetadata,
    DeleteBlob,
    UndeleteBlob,
    SetBlobProperties,
    SetBlobMetadata,
    SnapshotBlob,
    LeaseBlob,
    SetBlobTier,
    GetBlobTags,
    SetBlobTags,
    PutBlock,
    PutBlockFromUrl,
    PutBlockList,
    GetBlockList,
    AppendBlock,
    AppendBlockFromUrl,
    SealBlob,
    PutPage,
    PutPageFromUrl,
    GetPageRanges,
    SetImmutabilityPolicy,
    DeleteImmutabilityPolicy,
    SetLegalHold,
    SetExpiry,
    QueryBlob,
    Options,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Any resource type of an account SAS is fine.
    Any,
    Service,
    Container,
    Object,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anon {
    /// Never without credentials.
    No,
    /// Allowed when the container's public access is `blob` or `container`.
    Blob,
    /// Allowed when the container's public access is `container`.
    Container,
}

pub struct Need {
    pub level: Level,
    /// SAS permission letters; any one of them is enough. Empty: any valid SAS.
    pub perms: &'static str,
    /// Letters to use instead when the blob does not exist yet.
    pub perms_new: Option<&'static str>,
    pub anon: Anon,
}

const fn need(level: Level, perms: &'static str, anon: Anon) -> Need {
    Need { level, perms, perms_new: None, anon }
}

const fn need_new(level: Level, perms: &'static str, perms_new: &'static str) -> Need {
    Need { level, perms, perms_new: Some(perms_new), anon: Anon::No }
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::ListContainers => "List Containers",
            Op::GetServiceProperties => "Get Blob Service Properties",
            Op::SetServiceProperties => "Set Blob Service Properties",
            Op::GetServiceStats => "Get Blob Service Stats",
            Op::GetUserDelegationKey => "Get User Delegation Key",
            Op::GetAccountInfo => "Get Account Information",
            Op::FindBlobsAccount => "Find Blobs by Tags",
            Op::BatchAccount | Op::BatchContainer => "Blob Batch",
            Op::CreateContainer => "Create Container",
            Op::DeleteContainer => "Delete Container",
            Op::GetContainerProperties => "Get Container Properties",
            Op::GetContainerMetadata => "Get Container Metadata",
            Op::SetContainerMetadata => "Set Container Metadata",
            Op::GetContainerAcl => "Get Container ACL",
            Op::SetContainerAcl => "Set Container ACL",
            Op::LeaseContainer => "Lease Container",
            Op::ListBlobs => "List Blobs",
            Op::FindBlobsContainer => "Find Blobs by Tags in Container",
            Op::UndeleteContainer => "Restore Container",
            Op::RenameContainer => "Rename Container",
            Op::CreateSession => "Create Container Session",
            Op::PutBlob => "Put Blob",
            Op::PutBlobFromUrl => "Put Blob From URL",
            Op::CopyBlob => "Copy Blob",
            Op::AbortCopy => "Abort Copy Blob",
            Op::IncrementalCopy => "Incremental Copy Blob",
            Op::GetBlob => "Get Blob",
            Op::GetBlobProperties => "Get Blob Properties",
            Op::GetBlobMetadata => "Get Blob Metadata",
            Op::DeleteBlob => "Delete Blob",
            Op::UndeleteBlob => "Undelete Blob",
            Op::SetBlobProperties => "Set Blob Properties",
            Op::SetBlobMetadata => "Set Blob Metadata",
            Op::SnapshotBlob => "Snapshot Blob",
            Op::LeaseBlob => "Lease Blob",
            Op::SetBlobTier => "Set Blob Tier",
            Op::GetBlobTags => "Get Blob Tags",
            Op::SetBlobTags => "Set Blob Tags",
            Op::PutBlock => "Put Block",
            Op::PutBlockFromUrl => "Put Block From URL",
            Op::PutBlockList => "Put Block List",
            Op::GetBlockList => "Get Block List",
            Op::AppendBlock => "Append Block",
            Op::AppendBlockFromUrl => "Append Block From URL",
            Op::SealBlob => "Append Blob Seal",
            Op::PutPage => "Put Page",
            Op::PutPageFromUrl => "Put Page From URL",
            Op::GetPageRanges => "Get Page Ranges",
            Op::SetImmutabilityPolicy => "Set Blob Immutability Policy",
            Op::DeleteImmutabilityPolicy => "Delete Blob Immutability Policy",
            Op::SetLegalHold => "Set Blob Legal Hold",
            Op::SetExpiry => "Set Blob Expiry",
            Op::QueryBlob => "Query Blob Contents",
            Op::Options => "Preflight Blob Request",
            Op::Unknown => "unknown operation",
        }
    }

    pub fn need(self) -> Need {
        match self {
            Op::ListContainers => need(Level::Service, "l", Anon::No),
            Op::GetServiceProperties | Op::GetServiceStats => need(Level::Service, "r", Anon::No),
            Op::SetServiceProperties => need(Level::Service, "w", Anon::No),
            Op::GetUserDelegationKey => need(Level::Service, "", Anon::No),
            Op::GetAccountInfo => need(Level::Any, "", Anon::No),
            Op::FindBlobsAccount => need(Level::Service, "f", Anon::No),
            Op::BatchAccount => need(Level::Service, "", Anon::No),
            Op::CreateContainer => need(Level::Container, "cw", Anon::No),
            Op::DeleteContainer => need(Level::Container, "d", Anon::No),
            Op::GetContainerProperties | Op::GetContainerMetadata => need(Level::Container, "r", Anon::Container),
            Op::GetContainerAcl => need(Level::Container, "r", Anon::No),
            Op::SetContainerMetadata | Op::SetContainerAcl | Op::LeaseContainer => need(Level::Container, "w", Anon::No),
            Op::ListBlobs => need(Level::Container, "l", Anon::Container),
            Op::FindBlobsContainer => need(Level::Container, "f", Anon::No),
            Op::BatchContainer => need(Level::Container, "", Anon::No),
            Op::UndeleteContainer | Op::RenameContainer => need(Level::Container, "w", Anon::No),
            Op::CreateSession => need(Level::Container, "r", Anon::No),
            Op::PutBlob | Op::PutBlobFromUrl | Op::CopyBlob | Op::PutBlock | Op::PutBlockFromUrl | Op::PutBlockList => {
                need_new(Level::Object, "w", "cw")
            }
            Op::AppendBlock | Op::AppendBlockFromUrl => need_new(Level::Object, "aw", "aw"),
            Op::PutPage | Op::PutPageFromUrl => need(Level::Object, "w", Anon::No),
            Op::AbortCopy | Op::IncrementalCopy => need(Level::Object, "w", Anon::No),
            Op::GetBlob | Op::GetBlobProperties | Op::GetBlobMetadata | Op::GetBlockList | Op::GetPageRanges => {
                need(Level::Object, "r", Anon::Blob)
            }
            Op::DeleteBlob => need(Level::Object, "d", Anon::No),
            Op::UndeleteBlob => need(Level::Object, "wd", Anon::No),
            Op::SetBlobProperties | Op::SetBlobMetadata | Op::LeaseBlob | Op::SetBlobTier | Op::SealBlob | Op::SetExpiry => {
                need(Level::Object, "w", Anon::No)
            }
            Op::SnapshotBlob => need(Level::Object, "cw", Anon::No),
            Op::GetBlobTags | Op::SetBlobTags => need(Level::Object, "t", Anon::No),
            Op::SetImmutabilityPolicy | Op::DeleteImmutabilityPolicy | Op::SetLegalHold => need(Level::Object, "i", Anon::No),
            Op::QueryBlob => need(Level::Object, "r", Anon::No),
            Op::Options | Op::Unknown => need(Level::Service, "", Anon::Container),
        }
    }
}

/// Identify the operation. `depth`: 0 service, 1 container, 2 blob.
pub fn identify(req: &Request, depth: u8) -> Op {
    let m = req.method.as_str();
    let q = &req.query;
    let comp = q.get("comp").map(|s| s.to_ascii_lowercase());
    let comp = comp.as_deref();
    let restype = q.get("restype").map(|s| s.to_ascii_lowercase());
    let restype = restype.as_deref();
    let has_copy_source = req.header("x-ms-copy-source").is_some();

    if m == "OPTIONS" {
        return Op::Options;
    }
    match depth {
        0 => match (m, restype, comp) {
            ("GET", Some("service"), Some("properties")) => Op::GetServiceProperties,
            ("PUT", Some("service"), Some("properties")) => Op::SetServiceProperties,
            ("GET", Some("service"), Some("stats")) => Op::GetServiceStats,
            ("POST", Some("service"), Some("userdelegationkey")) => Op::GetUserDelegationKey,
            ("GET" | "HEAD", Some("account"), Some("properties")) => Op::GetAccountInfo,
            ("GET", _, Some("list")) => Op::ListContainers,
            ("GET", _, Some("blobs")) => Op::FindBlobsAccount,
            ("POST", _, Some("batch")) => Op::BatchAccount,
            _ => Op::Unknown,
        },
        1 => match (m, restype, comp) {
            ("GET" | "HEAD", Some("account"), Some("properties")) => Op::GetAccountInfo,
            ("PUT", Some("container"), None) => Op::CreateContainer,
            ("DELETE", Some("container"), None) => Op::DeleteContainer,
            ("GET" | "HEAD", Some("container"), None) => Op::GetContainerProperties,
            ("GET" | "HEAD", Some("container"), Some("metadata")) => Op::GetContainerMetadata,
            ("PUT", Some("container"), Some("metadata")) => Op::SetContainerMetadata,
            ("GET" | "HEAD", Some("container"), Some("acl")) => Op::GetContainerAcl,
            ("PUT", Some("container"), Some("acl")) => Op::SetContainerAcl,
            ("PUT", Some("container"), Some("lease")) => Op::LeaseContainer,
            ("GET", Some("container"), Some("list")) => Op::ListBlobs,
            ("GET", Some("container"), Some("blobs")) => Op::FindBlobsContainer,
            ("POST", Some("container"), Some("batch")) => Op::BatchContainer,
            ("PUT", Some("container"), Some("undelete")) => Op::UndeleteContainer,
            ("PUT", Some("container"), Some("rename")) => Op::RenameContainer,
            ("POST", Some("container"), Some("session")) => Op::CreateSession,
            _ => Op::Unknown,
        },
        _ => match (m, restype, comp) {
            ("GET" | "HEAD", Some("account"), Some("properties")) => Op::GetAccountInfo,
            ("PUT", _, None) if has_copy_source => {
                if req.header("x-ms-blob-type").is_some() {
                    Op::PutBlobFromUrl
                } else {
                    Op::CopyBlob
                }
            }
            ("PUT", _, None) => Op::PutBlob,
            ("GET", _, None) => Op::GetBlob,
            ("HEAD", _, None) => Op::GetBlobProperties,
            ("DELETE", _, None) => Op::DeleteBlob,
            ("PUT", _, Some("metadata")) => Op::SetBlobMetadata,
            ("GET" | "HEAD", _, Some("metadata")) => Op::GetBlobMetadata,
            ("PUT", _, Some("properties")) => Op::SetBlobProperties,
            ("PUT", _, Some("snapshot")) => Op::SnapshotBlob,
            ("PUT", _, Some("lease")) => Op::LeaseBlob,
            ("PUT", _, Some("undelete")) => Op::UndeleteBlob,
            ("PUT", _, Some("tier")) => Op::SetBlobTier,
            ("GET", _, Some("tags")) => Op::GetBlobTags,
            ("PUT", _, Some("tags")) => Op::SetBlobTags,
            ("PUT", _, Some("block")) if has_copy_source => Op::PutBlockFromUrl,
            ("PUT", _, Some("block")) => Op::PutBlock,
            ("PUT", _, Some("blocklist")) => Op::PutBlockList,
            ("GET", _, Some("blocklist")) => Op::GetBlockList,
            ("PUT", _, Some("appendblock")) if has_copy_source => Op::AppendBlockFromUrl,
            ("PUT", _, Some("appendblock")) => Op::AppendBlock,
            ("PUT", _, Some("seal")) => Op::SealBlob,
            ("PUT", _, Some("page")) if has_copy_source => Op::PutPageFromUrl,
            ("PUT", _, Some("page")) => Op::PutPage,
            ("GET", _, Some("pagelist")) => Op::GetPageRanges,
            ("PUT", _, Some("copy")) => Op::AbortCopy,
            ("PUT", _, Some("incrementalcopy")) => Op::IncrementalCopy,
            ("PUT", _, Some("immutabilitypolicies")) => Op::SetImmutabilityPolicy,
            ("DELETE", _, Some("immutabilitypolicies")) => Op::DeleteImmutabilityPolicy,
            ("PUT", _, Some("legalhold")) => Op::SetLegalHold,
            ("PUT", _, Some("expiry")) => Op::SetExpiry,
            ("POST", _, Some("query")) => Op::QueryBlob,
            _ => Op::Unknown,
        },
    }
}
