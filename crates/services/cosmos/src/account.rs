//! `GET /`: the database account document.

use serde_json::{json, Value};

use crate::out::{Cx, Out};
use crate::state::Account;

pub fn read(cx: &Cx, account: &str, _acct: &Account) -> Out {
    // Every URL points back at the host the client used.
    let base = format!("{}/", cx.req.ctx.endpoint.base_url());
    let regions = json!([{ "name": "Local", "databaseAccountEndpoint": base }]);
    let qec = json!({
        "maxSqlQueryInputLength": 524288,
        "maxJoinsPerSqlQuery": 10,
        "maxLogicalAndPerSqlQuery": 500,
        "maxLogicalOrPerSqlQuery": 500,
        "maxUdfRefPerSqlQuery": 10,
        "maxInExpressionItemsCount": 16000,
        "queryMaxInMemorySortDocumentCount": 500,
        "maxQueryRequestTimeoutFraction": 0.9,
        "sqlAllowNonFiniteNumbers": false,
        "sqlAllowAggregateFunctions": true,
        "sqlAllowSubQuery": true,
        "sqlAllowScalarSubQuery": true,
        "allowNewKeywords": true,
        "sqlAllowLike": true,
        "sqlAllowGroupByClause": true,
        "maxSpatialQueryCells": 12,
        "spatialMaxGeometryPointCount": 256,
        "sqlDisableOptimizationFlags": 0,
        "preventDistinctBinaryRewrite": false,
        "enableSpatialIndexing": true,
        "enableOrderByFixes": true,
        "maxConcurrentQueryExecutions": 0,
        "queryEngineVersion": 2
    });
    let v: Value = json!({
        "_self": "",
        "id": account,
        "_rid": format!("{account}.documents.azure.com"),
        "media": "//media/",
        "addresses": "//addresses/",
        "_dbs": "//dbs/",
        "writableLocations": regions,
        "readableLocations": regions,
        "enableMultipleWriteLocations": false,
        "continuousBackupEnabled": false,
        "enableNRegionSynchronousCommit": false,
        "userReplicationPolicy": {
            "asyncReplication": false,
            "minReplicaSetSize": 1,
            "maxReplicasetSize": 1
        },
        "userConsistencyPolicy": { "defaultConsistencyLevel": "Session" },
        "systemReplicationPolicy": { "minReplicaSetSize": 1, "maxReplicasetSize": 1 },
        "readPolicy": { "primaryReadCoefficient": 1, "secondaryReadCoefficient": 1 },
        "queryEngineConfiguration": qec.to_string()
    });
    let mut v = v;
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "consistencyPolicy".into(),
            json!({
                "defaultConsistencyLevel": "Session",
                "maxStalenessPrefix": 100,
                "maxIntervalInSeconds": 5
            }),
        );
    }
    Out::json(200, v).header("x-ms-max-media-storage-usage-mb", "10240").header("x-ms-media-storage-usage-mb", "0")
}
