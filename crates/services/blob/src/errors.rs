//! Error constructors: one place that maps an Azure error code to a status and a
//! message. Messages are copied from the real recordings (or the REST docs).

use azemu_core::resp::Resp;
use azemu_core::storage;
use azemu_core::{RequestCtx, Response};

/// What a handler returns.
pub type ApiResult = Result<Response, ApiError>;

/// An Azure Storage error. Turned into the XML error body at the top of `handle`.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    /// Extra XML elements after `Message`.
    pub details: Vec<(String, String)>,
    /// Extra response headers.
    pub headers: Vec<(String, String)>,
    /// For a 501: the operation that is not built, as `GET /_azemu/unsupported` shows it (`Blob_Query (Parquet input)`).
    pub unsupported: Option<Box<str>>,
}

/// (code, status, message)
const TABLE: &[(&str, u16, &str)] = &[
    ("AccountIsDisabled", 403, "The specified account is disabled."),
    ("AppendPositionConditionNotMet", 412, "The append position condition specified was not met."),
    ("AuthenticationFailed", 403, "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature."),
    ("AuthorizationFailure", 403, "This request is not authorized to perform this operation."),
    ("AuthorizationPermissionMismatch", 403, "This request is not authorized to perform this operation using this permission."),
    ("AuthorizationProtocolMismatch", 403, "This request is not authorized to perform this operation using this protocol."),
    ("AuthorizationResourceTypeMismatch", 403, "This request is not authorized to perform this operation using this resource type."),
    ("AuthorizationServiceMismatch", 403, "This request is not authorized to perform this operation using this service."),
    ("AuthorizationSourceIPMismatch", 403, "This request is not authorized to perform this operation using this source IP."),
    ("BlobAlreadyExists", 409, "The specified blob already exists."),
    ("BlobArchived", 409, "This operation is not permitted on an archived blob."),
    ("BlobBeingRehydrated", 409, "This operation is not permitted because the blob is being rehydrated."),
    ("BlobImmutableDueToLegalHold", 409, "This operation is not permitted as the blob is immutable due to one or more legal holds."),
    ("BlobImmutableDueToPolicy", 409, "This operation is not permitted as the blob is immutable due to policy."),
    ("BlobIsSealed", 409, "The blob is sealed and its contents cannot be modified."),
    ("BlobNotFound", 404, "The specified blob does not exist."),
    ("BlobOverwritten", 409, "The blob has been recreated since the source was specified."),
    ("BlobUsesCustomerSpecifiedEncryption", 409, "The blob is encrypted with customer specified encryption, but it was not provided in the request."),
    ("BlockCountExceedsLimit", 409, "The committed block count cannot exceed the maximum limit of 50,000 blocks."),
    ("CannotVerifyCopySource", 404, "Could not verify the copy source within the specified time."),
    ("ConditionHeadersNotSupported", 400, "Condition headers are not supported."),
    ("ConditionNotMet", 412, "The condition specified using HTTP conditional header(s) is not met."),
    ("ContainerAlreadyExists", 409, "The specified container already exists."),
    ("ContainerBeingDeleted", 409, "The specified container is being deleted. Try operation later."),
    ("ContainerHasLegalHold", 409, "The requested operation is not allowed as the container has one or more legal holds."),
    ("ContainerImmutabilityPolicyLocked", 409, "The requested operation is not allowed as the container has a locked immutability policy."),
    ("ContainerNotFound", 404, "The specified container does not exist."),
    ("CopyIdMismatch", 409, "The specified copy ID did not match the copy ID for the pending copy operation."),
    ("CorsPreflightFailure", 403, "CORS not enabled or no matching rule found for this request."),
    ("EmptyMetadataKey", 400, "The key for one of the metadata key-value pairs is empty."),
    ("FeatureVersionMismatch", 409, "Feature version mismatch."),
    ("IncrementalCopyBlobMismatch", 409, "The specified source blob is different than the copy source of the existing incremental copy blob."),
    ("InfiniteLeaseDurationRequired", 412, "A lease ID was specified, but the lease for the blob has expired."),
    ("InternalError", 500, "The server encountered an internal error. Please retry the request."),
    ("InvalidAuthenticationInfo", 400, "Authentication information is not given in the correct format. Check the value of Authorization header."),
    ("InvalidBlobOrBlock", 400, "The specified blob or block content is invalid."),
    ("InvalidBlobType", 409, "The blob type is invalid for this operation."),
    ("InvalidBlockId", 400, "The specified blob or block content is invalid."),
    ("InvalidBlockList", 400, "The specified block list is invalid."),
    ("InvalidHeaderValue", 400, "The value for one of the HTTP headers is not in the correct format."),
    ("InvalidHttpVerb", 400, "The HTTP verb specified is invalid - it is not recognized by the server."),
    ("InvalidInput", 400, "One of the request inputs is not valid."),
    ("InvalidMd5", 400, "The MD5 value specified in the request is invalid. The MD5 value must be 128 bits and Base64-encoded."),
    ("InvalidMetadata", 400, "The metadata specified is invalid. It has characters that are not permitted."),
    ("InvalidOperation", 400, "The operation is not valid for the current state of the resource."),
    ("InvalidPageRange", 416, "The page range specified is invalid."),
    ("InvalidQueryParameterValue", 400, "Value for one of the query parameters specified in the request URI is invalid."),
    ("InvalidRange", 416, "The range specified is invalid for the current size of the resource."),
    ("InvalidResourceName", 400, "The specifed resource name contains invalid characters."),
    ("InvalidSourceBlobType", 409, "The copy source blob type is invalid for this operation."),
    ("InvalidSourceBlobUrl", 409, "The source URL for incremental copy request must be valid Azure Storage blob URL."),
    ("InvalidUri", 400, "The requested URI does not represent any resource on the server."),
    ("InvalidVersionForPageBlobOperation", 400, "All operations on page blobs require at least version 2009-09-19."),
    ("InvalidXmlDocument", 400, "XML specified is not syntactically valid."),
    ("InvalidXmlNodeValue", 400, "XML specified is not valid."),
    ("LeaseAlreadyBroken", 409, "The lease has already been broken and cannot be broken again."),
    ("LeaseAlreadyPresent", 409, "There is already a lease present."),
    ("LeaseIdMismatchWithBlobOperation", 412, "The lease ID specified did not match the lease ID for the blob."),
    ("LeaseIdMismatchWithContainerOperation", 412, "The lease ID specified did not match the lease ID for the container."),
    ("LeaseIdMismatchWithLeaseOperation", 409, "The lease ID specified did not match the lease ID for the blob."),
    ("LeaseIdMissing", 412, "There is currently a lease on the blob and no lease ID was specified in the request."),
    ("LeaseIsBreakingAndCannotBeAcquired", 409, "The lease ID matched, but the lease is currently in breaking state and cannot be acquired until it is broken."),
    ("LeaseIsBreakingAndCannotBeChanged", 409, "The lease ID matched, but the lease is currently in breaking state and cannot be changed."),
    ("LeaseIsBrokenAndCannotBeRenewed", 409, "The lease ID matched, but the lease has been broken explicitly and cannot be renewed."),
    ("LeaseLost", 412, "A lease ID was specified, but the lease for the blob has expired."),
    ("LeaseNotPresentWithBlobOperation", 412, "There is currently no lease on the blob."),
    ("LeaseNotPresentWithContainerOperation", 412, "There is currently no lease on the container."),
    ("LeaseNotPresentWithLeaseOperation", 409, "There is currently no lease on the blob."),
    ("MaxBlobSizeConditionNotMet", 412, "The max blob size condition specified was not met."),
    ("Md5Mismatch", 400, "The MD5 value specified in the request did not match with the MD5 value calculated by the server."),
    ("MetadataTooLarge", 400, "The metadata specified exceeds the maximum permissible size."),
    ("MissingRequiredHeader", 400, "An HTTP header that's mandatory for this request is not specified."),
    ("MissingRequiredQueryParameter", 400, "A query parameter that's mandatory for this request is not specified."),
    ("MissingRequiredXmlNode", 400, "An XML node that's mandatory for this request is not specified."),
    ("MultipleConditionHeadersNotSupported", 400, "Multiple condition headers are not supported."),
    ("NoAuthenticationInformation", 401, "Server failed to authenticate the request. Please refer to the information in the www-authenticate header."),
    ("NoPendingCopyOperation", 409, "There is currently no pending copy operation."),
    ("NotImplemented", 501, "The requested operation is not implemented on the specified resource."),
    ("OperationNotAllowedOnIncrementalCopyBlob", 409, "The specified blob is an incremental copy blob."),
    ("OutOfRangeInput", 400, "One of the request inputs is out of range."),
    ("OutOfRangeQueryParameterValue", 400, "One of the query parameters specified in the request URI is outside the permissible range."),
    ("PendingCopyOperation", 409, "There is currently a pending copy operation."),
    ("PublicAccessNotPermitted", 409, "Public access is not permitted on this storage account."),
    ("PreviousSnapshotNotFound", 409, "The previous snapshot specified for the Get Page Ranges diff operation was not found."),
    ("PreviousSnapshotCannotBeNewer", 400, "The prevsnapshot query parameter value cannot be newer than snapshot query parameter value."),
    ("RequestForbiddenByContainerEncryptionPolicy", 403, "The request is forbidden by the container encryption policy."),
    ("RequestBodyTooLarge", 413, "The request body is too large and exceeds the maximum permissible limit."),
    ("ResourceNotFound", 404, "The specified resource does not exist."),
    ("ResourceTypeMismatch", 409, "The resource type specified in the request does not match the type of the existing resource."),
    ("SequenceNumberConditionNotMet", 412, "The sequence number condition specified was not met."),
    ("SequenceNumberIncrementTooLarge", 409, "The sequence number increment cannot be performed because it would result in overflow of the sequence number."),
    ("SnapshotsPresent", 409, "This operation is not permitted because the blob has snapshots."),
    ("SourceConditionNotMet", 412, "The source condition specified using HTTP conditional header(s) is not met."),
    ("TargetConditionNotMet", 412, "The target condition specified using HTTP conditional header(s) is not met."),
    ("TagsTooLarge", 400, "The tags specified are invalid. It has characters that are not permitted or too large."),
    ("UnauthorizedBlobOverwrite", 403, "This request is not authorized to perform blob overwrites."),
    ("UnsupportedHeader", 400, "One of the HTTP headers specified in the request is not supported."),
    ("UnsupportedHttpVerb", 405, "The resource doesn't support specified Http Verb."),
    ("UnsupportedQueryParameter", 400, "One of the query parameters specified in the request URI is not supported."),
    ("UnsupportedXmlNode", 400, "One of the XML nodes specified in the request body is not supported."),
    ("AccessTierChangeTimeConditionNotMet", 412, "The condition specified using access tier change time conditional header(s) is not met."),
];

impl ApiError {
    /// Error for a known code. An unknown code becomes a 400 with a generic message.
    pub fn new(code: &str) -> ApiError {
        for (c, status, msg) in TABLE {
            if *c == code {
                return ApiError {
                    status: *status,
                    code: code.to_string(),
                    message: (*msg).to_string(),
                    details: vec![],
                    headers: vec![],
                    unsupported: None,
                };
            }
        }
        ApiError {
            status: 400,
            code: code.to_string(),
            message: "The request is invalid.".to_string(),
            details: vec![],
            headers: vec![],
            unsupported: None,
        }
    }

    pub fn msg(mut self, m: impl Into<String>) -> ApiError {
        self.message = m.into();
        self
    }

    pub fn status(mut self, s: u16) -> ApiError {
        self.status = s;
        self
    }

    pub fn detail(mut self, k: &str, v: impl Into<String>) -> ApiError {
        self.details.push((k.to_string(), v.into()));
        self
    }

    pub fn header(mut self, k: &str, v: impl Into<String>) -> ApiError {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    /// Operation is not built: 501 naming it. `GET /_azemu/unsupported` shows it as `Blob_<op without spaces>`.
    pub fn not_implemented(op: &str) -> ApiError {
        ApiError::new("NotImplemented")
            .msg(format!("blob: {op} is not implemented by this emulator."))
            .unsupported(format!("Blob_{}", op.replace(' ', "")))
    }

    /// Name the operation of a 501 answer for `GET /_azemu/unsupported` (see `azemu_core::unsupported`).
    pub fn unsupported(mut self, operation: impl Into<String>) -> ApiError {
        self.unsupported = Some(operation.into().into_boxed_str());
        self
    }

    /// 304 answer to a failed read condition (no body, error code header only).
    pub fn not_modified() -> ApiError {
        ApiError {
            status: 304,
            code: "ConditionNotMet".into(),
            message: String::new(),
            details: vec![],
            headers: vec![],
            unsupported: None,
        }
    }

    pub fn missing_header(name: &str) -> ApiError {
        ApiError::new("MissingRequiredHeader").detail("HeaderName", name)
    }

    pub fn invalid_header(name: &str, value: &str) -> ApiError {
        ApiError::new("InvalidHeaderValue").detail("HeaderName", name).detail("HeaderValue", value)
    }

    pub fn invalid_query(name: &str, value: &str) -> ApiError {
        ApiError::new("InvalidQueryParameterValue").detail("QueryParameterName", name).detail("QueryParameterValue", value)
    }

    pub fn out_of_range_query(name: &str, value: &str) -> ApiError {
        ApiError::new("OutOfRangeQueryParameterValue").detail("QueryParameterName", name).detail("QueryParameterValue", value)
    }

    /// The positions are not tracked; Azure always sends them, clients ignore them.
    pub fn invalid_xml(reason: &str) -> ApiError {
        ApiError::new("InvalidXmlDocument").detail("LineNumber", "1").detail("LinePosition", "1").detail("Reason", reason)
    }

    pub fn into_response(self, ctx: &RequestCtx) -> Response {
        if self.status == 304 {
            let mut r = Resp::new(304u16).header("x-ms-error-code", &self.code).header("Content-Length", "0");
            for (k, v) in &self.headers {
                r = r.header(k, v);
            }
            return r.build();
        }
        let details: Vec<(&str, &str)> = self.details.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut resp = storage::error_with_details(ctx, self.status, &self.code, &self.message, &details);
        if let (501, Some(op)) = (self.status, &self.unsupported) {
            resp = azemu_core::unsupported::mark(resp, op.to_string());
        }
        for (k, v) in &self.headers {
            if let (Ok(n), Ok(val)) = (azemu_core::http::HeaderName::from_bytes(k.as_bytes()), azemu_core::http::HeaderValue::from_str(v)) {
                resp.headers_mut().insert(n, val);
            }
        }
        resp
    }
}

/// Shorthands used everywhere.
pub fn err(code: &str) -> ApiError {
    ApiError::new(code)
}

pub fn blob_not_found() -> ApiError {
    ApiError::new("BlobNotFound")
}

pub fn container_not_found() -> ApiError {
    ApiError::new("ContainerNotFound")
}
