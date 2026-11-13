mod common;
use common::*;

#[tokio::test]
async fn container_lifecycle_and_errors() {
    let e = start().await;
    let r = e.send("PUT", "/cnt1?restype=container", &[("x-ms-meta-owner", "me")], "").await;
    assert_eq!(r.status, 201);
    assert!(r.header("etag").unwrap().starts_with('"'));
    assert!(r.header("last-modified").is_some());
    assert!(r.header("x-ms-request-id").is_some());
    assert_eq!(r.header("server"), Some("Windows-Azure-Blob/1.0 Microsoft-HTTPAPI/2.0"));

    let r = e.send("PUT", "/cnt1?restype=container", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "ContainerAlreadyExists"));
    assert!(r.text().contains("<Code>ContainerAlreadyExists</Code>"));

    let r = e.send("HEAD", "/cnt1?restype=container", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-meta-owner"), Some("me"));
    assert_eq!(r.header("x-ms-lease-state"), Some("available"));

    let r = e.send("GET", "/nope?restype=container", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "ContainerNotFound"));

    // names
    assert_eq!(code(&e.send("PUT", "/Bad_Name?restype=container", &[], "").await), "InvalidResourceName");
    assert_eq!(code(&e.send("PUT", "/ab?restype=container", &[], "").await), "OutOfRangeInput");

    let r = e.send("DELETE", "/cnt1?restype=container", &[], "").await;
    assert_eq!(r.status, 202);
    assert_eq!(e.send("DELETE", "/cnt1?restype=container", &[], "").await.status, 404);
}

#[tokio::test]
async fn list_containers_prefix_paging_and_metadata() {
    let e = start().await;
    for n in ["pax", "pbx", "pcx", "otherx"] {
        let n = n.to_string();
        e.send("PUT", &format!("/{n}?restype=container"), &[("x-ms-meta-k", "v")], "").await;
    }
    let r = e.send("GET", "/?comp=list&prefix=p&maxresults=2&include=metadata", &[], "").await;
    assert_eq!(r.status, 200);
    let body = r.text();
    assert_eq!(tags(&body, "Name"), ["pax", "pbx"]);
    assert!(body.contains("<k>v</k>"));
    let next = tags(&body, "NextMarker").remove(0);
    assert!(!next.is_empty());
    let r = e.send("GET", &format!("/?comp=list&prefix=p&maxresults=2&marker={}", next.replace('/', "%2F")), &[], "").await;
    assert_eq!(tags(&r.text(), "Name"), ["pcx"]);
    assert!(r.text().contains("<NextMarker />"));
    // bad maxresults
    let r = e.send("GET", "/?comp=list&maxresults=0", &[], "").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn container_acl_roundtrip_and_public_access() {
    let e = start().await;
    e.create_container("acl").await;
    let body = "<?xml version=\"1.0\"?><SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Start>2020-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>rl</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    let r = e.send("PUT", "/acl?restype=container&comp=acl", &[("x-ms-blob-public-access", "blob")], body).await;
    assert_eq!(r.status, 200);
    let r = e.send("GET", "/acl?restype=container&comp=acl", &[], "").await;
    assert_eq!(r.header("x-ms-blob-public-access"), Some("blob"));
    let t = r.text();
    assert_eq!(tags(&t, "Id"), ["p1"]);
    assert_eq!(tags(&t, "Start"), ["2020-01-01T00:00:00.0000000Z"]);
    assert_eq!(tags(&t, "Permission"), ["rl"]);
    // sending nothing clears both
    e.send("PUT", "/acl?restype=container&comp=acl", &[], "").await;
    let r = e.send("GET", "/acl?restype=container&comp=acl", &[], "").await;
    assert!(r.header("x-ms-blob-public-access").is_none());
    assert!(tags(&r.text(), "Id").is_empty());
}

#[tokio::test]
async fn service_properties_roundtrip_and_validation() {
    let e = start().await;
    let r = e.send("GET", "/?restype=service&comp=properties", &[], "").await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("<StorageServiceProperties>"));
    let set = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>*</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods><MaxAgeInSeconds>60</MaxAgeInSeconds><ExposedHeaders>x-ms-*</ExposedHeaders><AllowedHeaders>*</AllowedHeaders></CorsRule></Cors><DeleteRetentionPolicy><Enabled>true</Enabled><Days>3</Days></DeleteRetentionPolicy><StaticWebsite><Enabled>true</Enabled><IndexDocument>index.html</IndexDocument></StaticWebsite></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[], set).await.status, 202);
    let t = e.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(t.contains("<AllowedMethods>GET,PUT</AllowedMethods>"));
    assert!(t.contains("<Days>3</Days>"));
    assert!(t.contains("<IndexDocument>index.html</IndexDocument>"));
    // too many days
    let bad = "<StorageServiceProperties><DeleteRetentionPolicy><Enabled>true</Enabled><Days>366</Days></DeleteRetentionPolicy></StorageServiceProperties>";
    let r = e.send("PUT", "/?restype=service&comp=properties", &[], bad).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidXmlDocument"));
    // broken xml
    let r = e.send("PUT", "/?restype=service&comp=properties", &[], "<a><b></a>").await;
    assert_eq!(code(&r), "InvalidXmlDocument");
    // cors preflight
    let r = e.anon("OPTIONS", "/cnt/b", &[("Origin", "https://x.example"), ("Access-Control-Request-Method", "PUT")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    let r = e.anon("OPTIONS", "/cnt/b", &[("Origin", "https://x.example"), ("Access-Control-Request-Method", "DELETE")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "CorsPreflightFailure"));
}

#[tokio::test]
async fn account_information_and_root_container() {
    let e = start().await;
    let r = e.send("GET", "/?restype=account&comp=properties", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-account-kind"), Some("StorageV2"));
    e.create_container("$root").await;
    // a blob in $root is addressed without the container name
    let r = e.send("PUT", "/rootblob", &[("x-ms-blob-type", "BlockBlob")], "in root").await;
    assert_eq!(r.status, 201);
    assert_eq!(e.send("GET", "/%24root/rootblob", &[], "").await.text(), "in root");
    assert_eq!(e.send("GET", "/rootblob", &[], "").await.text(), "in root");
}
