//! API keys, the versions the emulator serves, and error codes.

/// One served API: `min..=max` are the versions in `ApiVersions`.
#[derive(Clone, Copy, Debug)]
pub struct ApiSpec {
    pub key: i16,
    pub name: &'static str,
    pub min: i16,
    pub max: i16,
}

pub const PRODUCE: i16 = 0;
pub const FETCH: i16 = 1;
pub const LIST_OFFSETS: i16 = 2;
pub const METADATA: i16 = 3;
pub const OFFSET_COMMIT: i16 = 8;
pub const OFFSET_FETCH: i16 = 9;
pub const FIND_COORDINATOR: i16 = 10;
pub const JOIN_GROUP: i16 = 11;
pub const HEARTBEAT: i16 = 12;
pub const LEAVE_GROUP: i16 = 13;
pub const SYNC_GROUP: i16 = 14;
pub const DESCRIBE_GROUPS: i16 = 15;
pub const LIST_GROUPS: i16 = 16;
pub const SASL_HANDSHAKE: i16 = 17;
pub const API_VERSIONS: i16 = 18;
pub const CREATE_TOPICS: i16 = 19;
pub const DELETE_TOPICS: i16 = 20;
pub const INIT_PRODUCER_ID: i16 = 22;
pub const DESCRIBE_CONFIGS: i16 = 32;
pub const SASL_AUTHENTICATE: i16 = 36;
pub const CREATE_PARTITIONS: i16 = 37;

/// The APIs and versions the emulator serves, in the order `ApiVersions` lists them. The upper bound of each is the
/// last version before the one that needs topic ids (Produce 13, Fetch 13, ...) or another feature the emulator
/// does not have; the lower bound is the oldest version that the current Java client (4.x) still sends.
pub static SERVED: &[ApiSpec] = &[
    ApiSpec { key: PRODUCE, name: "Produce", min: 3, max: 9 },
    ApiSpec { key: FETCH, name: "Fetch", min: 4, max: 12 },
    ApiSpec { key: LIST_OFFSETS, name: "ListOffsets", min: 1, max: 6 },
    ApiSpec { key: METADATA, name: "Metadata", min: 1, max: 9 },
    ApiSpec { key: OFFSET_COMMIT, name: "OffsetCommit", min: 2, max: 8 },
    ApiSpec { key: OFFSET_FETCH, name: "OffsetFetch", min: 1, max: 7 },
    ApiSpec { key: FIND_COORDINATOR, name: "FindCoordinator", min: 0, max: 3 },
    ApiSpec { key: JOIN_GROUP, name: "JoinGroup", min: 1, max: 7 },
    ApiSpec { key: HEARTBEAT, name: "Heartbeat", min: 0, max: 4 },
    ApiSpec { key: LEAVE_GROUP, name: "LeaveGroup", min: 0, max: 4 },
    ApiSpec { key: SYNC_GROUP, name: "SyncGroup", min: 0, max: 5 },
    ApiSpec { key: DESCRIBE_GROUPS, name: "DescribeGroups", min: 0, max: 5 },
    ApiSpec { key: LIST_GROUPS, name: "ListGroups", min: 0, max: 4 },
    ApiSpec { key: SASL_HANDSHAKE, name: "SaslHandshake", min: 1, max: 1 },
    ApiSpec { key: API_VERSIONS, name: "ApiVersions", min: 0, max: 4 },
    ApiSpec { key: CREATE_TOPICS, name: "CreateTopics", min: 0, max: 5 },
    ApiSpec { key: DELETE_TOPICS, name: "DeleteTopics", min: 0, max: 5 },
    ApiSpec { key: INIT_PRODUCER_ID, name: "InitProducerId", min: 0, max: 4 },
    ApiSpec { key: DESCRIBE_CONFIGS, name: "DescribeConfigs", min: 0, max: 4 },
    ApiSpec { key: SASL_AUTHENTICATE, name: "SaslAuthenticate", min: 0, max: 2 },
    ApiSpec { key: CREATE_PARTITIONS, name: "CreatePartitions", min: 0, max: 3 },
];

pub fn served(key: i16) -> Option<&'static ApiSpec> {
    SERVED.iter().find(|a| a.key == key)
}

/// Kafka error codes (`org.apache.kafka.common.protocol.Errors`).
pub mod err {
    pub const UNKNOWN_SERVER_ERROR: i16 = -1;
    pub const NONE: i16 = 0;
    pub const OFFSET_OUT_OF_RANGE: i16 = 1;
    pub const CORRUPT_MESSAGE: i16 = 2;
    pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub const INVALID_FETCH_SIZE: i16 = 4;
    pub const LEADER_NOT_AVAILABLE: i16 = 5;
    pub const NOT_LEADER_OR_FOLLOWER: i16 = 6;
    pub const REQUEST_TIMED_OUT: i16 = 7;
    pub const MESSAGE_TOO_LARGE: i16 = 10;
    pub const OFFSET_METADATA_TOO_LARGE: i16 = 12;
    pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;
    pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;
    pub const NOT_COORDINATOR: i16 = 16;
    pub const INVALID_TOPIC_EXCEPTION: i16 = 17;
    pub const RECORD_LIST_TOO_LARGE: i16 = 18;
    pub const INVALID_REQUIRED_ACKS: i16 = 21;
    pub const ILLEGAL_GENERATION: i16 = 22;
    pub const INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
    pub const INVALID_GROUP_ID: i16 = 24;
    pub const UNKNOWN_MEMBER_ID: i16 = 25;
    pub const INVALID_SESSION_TIMEOUT: i16 = 26;
    pub const REBALANCE_IN_PROGRESS: i16 = 27;
    pub const INVALID_COMMIT_OFFSET_SIZE: i16 = 28;
    pub const UNSUPPORTED_SASL_MECHANISM: i16 = 33;
    pub const ILLEGAL_SASL_STATE: i16 = 34;
    pub const UNSUPPORTED_VERSION: i16 = 35;
    pub const TOPIC_ALREADY_EXISTS: i16 = 36;
    pub const INVALID_PARTITIONS: i16 = 37;
    pub const INVALID_REPLICATION_FACTOR: i16 = 38;
    pub const INVALID_REQUEST: i16 = 42;
    pub const UNSUPPORTED_FOR_MESSAGE_FORMAT: i16 = 43;
    pub const OUT_OF_ORDER_SEQUENCE_NUMBER: i16 = 45;
    pub const DUPLICATE_SEQUENCE_NUMBER: i16 = 46;
    pub const INVALID_PRODUCER_EPOCH: i16 = 47;
    pub const INVALID_TXN_STATE: i16 = 48;
    pub const INVALID_RECORD: i16 = 87;
    pub const INVALID_CONFIG: i16 = 40;
    pub const UNKNOWN_PRODUCER_ID: i16 = 59;
    pub const GROUP_MAX_SIZE_REACHED: i16 = 81;
    pub const MEMBER_ID_REQUIRED: i16 = 79;
    pub const GROUP_ID_NOT_FOUND: i16 = 69;
    pub const UNSUPPORTED_COMPRESSION_TYPE: i16 = 76;
    pub const SASL_AUTHENTICATION_FAILED: i16 = 58;
    pub const THROTTLING_QUOTA_EXCEEDED: i16 = 89;
    pub const POLICY_VIOLATION: i16 = 44;
    pub const FENCED_INSTANCE_ID: i16 = 82;
}
