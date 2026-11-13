//! The broker's answers: connection state, `ApiVersions`, `Metadata`, SASL, the coordinator APIs and the admin APIs.
//! Produce and Fetch are in `produce.rs` and `fetch.rs`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use azemu_core::RawConn;
use azemu_kafka::api::{self, err};
use azemu_kafka::{Request, Struct, Value};
use bytes::Bytes;

use super::groups::{Committed, JoinRequest, SyncRequest, MAX_GROUP_ID_BYTES, MAX_OFFSET_METADATA_BYTES};
use super::KafkaConfig;
use crate::amqp_conn::namespace_from_hostname;
use crate::model::{DEFAULT_PARTITIONS, DEFAULT_RETENTION_MS, MAX_MESSAGE_SIZE};
use crate::state::{now_ms, Shared, DEFAULT_NAMESPACE};

pub struct Broker {
    pub(crate) shared: Arc<Shared>,
    pub(crate) cfg: KafkaConfig,
}

/// What one connection knows.
pub struct Conn {
    pub peer: std::net::SocketAddr,
    pub local: std::net::SocketAddr,
    /// TLS server name.
    pub sni: Option<String>,
    /// Host of the `Endpoint=sb://...` in the SASL PLAIN password.
    pub endpoint_host: Option<String>,
    pub client_id: String,
    pub sasl_mechanism: Option<String>,
}

impl Conn {
    pub fn new(c: &RawConn) -> Conn {
        Conn {
            peer: c.peer,
            local: c.local,
            sni: c.server_name.clone(),
            endpoint_host: None,
            client_id: String::new(),
            sasl_mechanism: None,
        }
    }

    /// The namespace: the TLS server name when it names one (`myns.servicebus.windows.net`), else the endpoint in the
    /// connection string, else `default`.
    pub fn namespace(&self) -> String {
        let from_sni = self.sni.as_deref().map(|s| namespace_from_hostname(Some(s))).filter(|n| n != DEFAULT_NAMESPACE);
        if let Some(n) = from_sni {
            return n;
        }
        if let Some(h) = &self.endpoint_host {
            return namespace_from_hostname(Some(h));
        }
        DEFAULT_NAMESPACE.to_string()
    }
}

/// A host the client can plausibly reach without help: an IP address, `localhost` names, a name with no dot.
fn reachable_looking(h: &str) -> bool {
    let h = h.to_ascii_lowercase();
    h.parse::<std::net::IpAddr>().is_ok() || h == "localhost" || h.ends_with(".localhost") || !h.contains('.')
}

/// Kafka topic names: letters, digits, `.`, `_`, `-`, at most 249 characters, not `.` or `..`.
pub fn valid_topic_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 249 && n != "." && n != ".." && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// What the connection does with a request.
pub enum Reply {
    /// Write this answer.
    Answer(Struct),
    /// Write nothing and read the next request (`acks=0`).
    Nothing,
    /// Close the connection (a request that cannot be handled).
    Close,
}

impl Broker {
    pub async fn handle(&self, c: &mut Conn, req: Request) -> Reply {
        if super::limits::names_too_much(&req.body) {
            tracing::debug!(api_key = req.header.api_key, "kafka: a request names too many topics or partitions, closing");
            return Reply::Close;
        }
        let b = &req.body;
        let v = req.header.version;
        if let Some(id) = &req.header.client_id {
            if c.client_id.is_empty() {
                c.client_id = id.clone();
            }
        }
        Reply::Answer(match req.header.api_key {
            api::API_VERSIONS => self.api_versions(err::NONE),
            api::METADATA => self.metadata(c, b),
            api::SASL_HANDSHAKE => self.sasl_handshake(c, b),
            api::SASL_AUTHENTICATE => self.sasl_authenticate(c, b),
            api::FIND_COORDINATOR => self.find_coordinator(c, b),
            api::INIT_PRODUCER_ID => self.init_producer_id(b),
            api::PRODUCE => return self.produce(c, b).map_or(Reply::Nothing, Reply::Answer),
            api::FETCH => self.fetch(c, b).await,
            api::LIST_OFFSETS => self.list_offsets(c, b),
            api::JOIN_GROUP => self.join_group(c, b, v).await,
            api::SYNC_GROUP => self.sync_group(c, b).await,
            api::HEARTBEAT => Struct::new().with(
                "ErrorCode",
                self.shared.kafka.heartbeat(
                    &c.namespace(),
                    b.str("GroupId").unwrap_or(""),
                    b.str("MemberId").unwrap_or(""),
                    b.i32("GenerationId"),
                ),
            ),
            api::LEAVE_GROUP => self.leave_group(c, b, v),
            api::OFFSET_COMMIT => self.offset_commit(c, b),
            api::OFFSET_FETCH => self.offset_fetch(c, b),
            api::DESCRIBE_GROUPS => self.describe_groups(c, b),
            api::LIST_GROUPS => self.list_groups(c, b, v),
            api::CREATE_TOPICS => self.create_topics(c, b),
            api::DELETE_TOPICS => self.delete_topics(c, b),
            api::CREATE_PARTITIONS => self.create_partitions(c, b),
            api::DESCRIBE_CONFIGS => self.describe_configs(c, b),
            _ => return Reply::Nothing,
        })
    }

    // ---- the one broker --------------------------------------------------------------------------------------------

    /// The address the client is told to use for the broker (and the coordinator): the setting, else the TLS server name,
    /// else the host of the endpoint in the connection string (when a client can plausibly reach it), else the address
    /// the connection came in on.
    pub fn advertised(&self, c: &Conn) -> (String, i32) {
        let host = self
            .cfg
            .advertised_host
            .clone()
            .or_else(|| c.sni.clone())
            .or_else(|| c.endpoint_host.clone().filter(|h| reachable_looking(h)))
            .unwrap_or_else(|| c.local.ip().to_string());
        let port = self.cfg.advertised_port.unwrap_or_else(|| c.local.port());
        (host, port as i32)
    }

    pub fn api_versions(&self, error: i16) -> Struct {
        let keys: Vec<Struct> =
            api::SERVED.iter().map(|a| Struct::new().with("ApiKey", a.key).with("MinVersion", a.min).with("MaxVersion", a.max)).collect();
        Struct::new().with("ErrorCode", error).with("ApiKeys", keys).with("ThrottleTimeMs", 0)
    }

    fn broker_struct(&self, c: &Conn) -> Struct {
        let (host, port) = self.advertised(c);
        Struct::new().with("NodeId", 0).with("Host", host).with("Port", port).with("Rack", Value::Null)
    }

    // ---- metadata ------------------------------------------------------------------------------------------------

    fn metadata(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let allow_auto = b.bool("AllowAutoTopicCreation");
        let now = now_ms();
        let mut topics = Vec::new();
        {
            let mut st = self.shared.state.lock();
            let requested = b.get("Topics");
            let names: Vec<String> = match requested {
                Value::Null => {
                    let n = st.resolve(&ns);
                    st.namespaces.get(&n).map(|n| n.hubs.values().map(|h| h.name.clone()).collect()).unwrap_or_default()
                }
                other => {
                    // a topic named twice is answered once (clients never do it; a request that does is not worth a copy per mention)
                    let mut seen = std::collections::HashSet::new();
                    other
                        .structs()
                        .filter_map(|t| t.str("Name"))
                        .filter(|n| seen.insert(n.to_ascii_lowercase()))
                        .map(str::to_string)
                        .collect()
                }
            };
            let mut listed = 0usize;
            for name in names {
                if st.hub(&ns, &name).is_none() {
                    if !valid_topic_name(&name) {
                        topics.push(topic_error(&name, err::INVALID_TOPIC_EXCEPTION));
                        continue;
                    }
                    if self.shared.autocreate && allow_auto {
                        if st.ensure_hub(&ns, &name, true, now).is_err() {
                            topics.push(topic_error(&name, err::UNKNOWN_TOPIC_OR_PARTITION));
                            continue;
                        }
                    } else {
                        topics.push(topic_error(&name, err::UNKNOWN_TOPIC_OR_PARTITION));
                        continue;
                    }
                }
                let Some(hub) = st.hub(&ns, &name) else {
                    topics.push(topic_error(&name, err::UNKNOWN_TOPIC_OR_PARTITION));
                    continue;
                };
                // an answer lists a bounded number of partitions; the topics past it get an error clients retry
                if listed + hub.partitions.len() > super::limits::MAX_PARTITIONS_PER_ANSWER {
                    topics.push(topic_error(&name, err::LEADER_NOT_AVAILABLE));
                    continue;
                }
                listed += hub.partitions.len();
                let parts: Vec<Struct> = (0..hub.partitions.len() as i32)
                    .map(|i| {
                        Struct::new()
                            .with("ErrorCode", 0)
                            .with("PartitionIndex", i)
                            .with("LeaderId", 0)
                            .with("LeaderEpoch", 0)
                            .with("ReplicaNodes", vec![0i32])
                            .with("IsrNodes", vec![0i32])
                            .with("OfflineReplicas", Vec::<i32>::new())
                    })
                    .collect();
                topics.push(Struct::new().with("ErrorCode", 0).with("Name", name).with("IsInternal", false).with("Partitions", parts));
            }
        }
        Struct::new()
            .with("ThrottleTimeMs", 0)
            .with("Brokers", vec![self.broker_struct(c)])
            .with("ClusterId", "azemu-eventhubs")
            .with("ControllerId", 0)
            .with("Topics", topics)
    }

    // ---- SASL ------------------------------------------------------------------------------------------------------

    fn sasl_handshake(&self, c: &mut Conn, b: &Struct) -> Struct {
        let m = b.str("Mechanism").unwrap_or("").to_string();
        let ok = m == "PLAIN" || m == "OAUTHBEARER";
        if ok {
            c.sasl_mechanism = Some(m);
        }
        Struct::new()
            .with("ErrorCode", if ok { err::NONE } else { err::UNSUPPORTED_SASL_MECHANISM })
            .with("Mechanisms", vec!["PLAIN".to_string(), "OAUTHBEARER".to_string()])
    }

    /// Any credentials are accepted (loose authentication). The shape is checked, and the namespace is taken from
    /// the `Endpoint=sb://...` of the connection string given as the PLAIN password.
    fn sasl_authenticate(&self, c: &mut Conn, b: &Struct) -> Struct {
        let bytes = b.bytes("AuthBytes").cloned().unwrap_or_default();
        let fail = |msg: &str| {
            Struct::new()
                .with("ErrorCode", err::SASL_AUTHENTICATION_FAILED)
                .with("ErrorMessage", msg)
                .with("AuthBytes", Bytes::new())
                .with("SessionLifetimeMs", 0i64)
        };
        match c.sasl_mechanism.as_deref() {
            Some("PLAIN") => {
                let parts: Vec<&[u8]> = bytes.split(|x| *x == 0).collect();
                if parts.len() != 3 {
                    return fail("Invalid SASL/PLAIN response: expected authzid, username and password separated by NUL");
                }
                let password = String::from_utf8_lossy(parts[2]).into_owned();
                c.endpoint_host = endpoint_host(&password);
            }
            Some("OAUTHBEARER") => {
                let text = String::from_utf8_lossy(&bytes);
                if !text.contains("auth=Bearer ") && !text.contains("auth=bearer ") {
                    return fail("Invalid OAUTHBEARER client first message: no auth=Bearer value");
                }
            }
            _ => {
                return Struct::new()
                    .with("ErrorCode", err::ILLEGAL_SASL_STATE)
                    .with("ErrorMessage", "SaslAuthenticate before SaslHandshake")
                    .with("AuthBytes", Bytes::new())
                    .with("SessionLifetimeMs", 0i64)
            }
        }
        Struct::new().with("ErrorCode", 0).with("ErrorMessage", Value::Null).with("AuthBytes", Bytes::new()).with("SessionLifetimeMs", 0i64)
    }

    // ---- coordinator -----------------------------------------------------------------------------------------------

    fn find_coordinator(&self, c: &Conn, b: &Struct) -> Struct {
        let key = b.str("Key").unwrap_or("");
        let (code, msg) = if b.i32("KeyType") != 0 {
            (err::COORDINATOR_NOT_AVAILABLE, "transaction coordinators are not built")
        } else if key.is_empty() || key.len() > MAX_GROUP_ID_BYTES {
            (err::INVALID_GROUP_ID, "the group id is empty or longer than 256 bytes")
        } else {
            (err::NONE, "")
        };
        let br = self.broker_struct(c);
        let mut s = Struct::new()
            .with("ThrottleTimeMs", 0)
            .with("ErrorCode", code)
            .with("ErrorMessage", if code == 0 { Value::Null } else { Value::Str(msg.to_string()) });
        if code == 0 {
            s.set("NodeId", br.i32("NodeId"));
            s.set("Host", br.str("Host").unwrap_or("").to_string());
            s.set("Port", br.i32("Port"));
        } else {
            s.set("NodeId", -1);
            s.set("Host", "");
            s.set("Port", -1);
        }
        s
    }

    fn init_producer_id(&self, b: &Struct) -> Struct {
        if b.get("TransactionalId").as_str().is_some() {
            return Struct::new()
                .with("ThrottleTimeMs", 0)
                .with("ErrorCode", err::UNSUPPORTED_VERSION)
                .with("ProducerId", -1i64)
                .with("ProducerEpoch", -1);
        }
        let (id, epoch) = if b.i64("ProducerId") >= 0 && b.i16("ProducerEpoch") >= 0 {
            // a producer that asks for a bumped epoch (KIP-360)
            (b.i64("ProducerId"), b.i16("ProducerEpoch").saturating_add(1))
        } else {
            {
                crate::model::next_revision();
                (self.shared.next_producer_id.fetch_add(1, Ordering::Relaxed), 0)
            }
        };
        Struct::new().with("ThrottleTimeMs", 0).with("ErrorCode", 0).with("ProducerId", id).with("ProducerEpoch", epoch)
    }

    async fn join_group(&self, c: &Conn, b: &Struct, version: i16) -> Struct {
        let protocols: Vec<(String, Bytes)> = b
            .structs("Protocols")
            .map(|p| (p.str("Name").unwrap_or("").to_string(), p.bytes("Metadata").cloned().unwrap_or_default()))
            .collect();
        let req = JoinRequest {
            group: b.str("GroupId").unwrap_or("").to_string(),
            member_id: b.str("MemberId").unwrap_or("").to_string(),
            instance_id: b.str("GroupInstanceId").map(str::to_string),
            client_id: c.client_id.clone(),
            client_host: format!("/{}", c.peer.ip()),
            session_timeout_ms: b.i32("SessionTimeoutMs"),
            rebalance_timeout_ms: b.i32("RebalanceTimeoutMs"),
            protocol_type: b.str("ProtocolType").unwrap_or("").to_string(),
            protocols,
            version,
        };
        let r = self.shared.kafka.join(&c.namespace(), req).await;
        let members: Vec<Struct> = r
            .members
            .into_iter()
            .map(|(id, inst, md)| Struct::new().with("MemberId", id).with("GroupInstanceId", inst).with("Metadata", md))
            .collect();
        Struct::new()
            .with("ThrottleTimeMs", 0)
            .with("ErrorCode", r.error)
            .with("GenerationId", r.generation)
            .with("ProtocolType", r.protocol_type)
            .with("ProtocolName", r.protocol)
            .with("Leader", r.leader)
            .with("MemberId", r.member_id)
            .with("Members", members)
    }

    async fn sync_group(&self, c: &Conn, b: &Struct) -> Struct {
        let req = SyncRequest {
            group: b.str("GroupId").unwrap_or("").to_string(),
            member_id: b.str("MemberId").unwrap_or("").to_string(),
            generation: b.i32("GenerationId"),
            protocol_type: b.str("ProtocolType").map(str::to_string),
            protocol: b.str("ProtocolName").map(str::to_string),
            assignments: b
                .structs("Assignments")
                .map(|a| (a.str("MemberId").unwrap_or("").to_string(), a.bytes("Assignment").cloned().unwrap_or_default()))
                .collect(),
        };
        let r = self.shared.kafka.sync(&c.namespace(), req).await;
        Struct::new()
            .with("ThrottleTimeMs", 0)
            .with("ErrorCode", r.error)
            .with("ProtocolType", r.protocol_type)
            .with("ProtocolName", r.protocol)
            .with("Assignment", r.assignment)
    }

    fn leave_group(&self, c: &Conn, b: &Struct, version: i16) -> Struct {
        let group = b.str("GroupId").unwrap_or("");
        let ns = c.namespace();
        if version < 3 {
            let r = self.shared.kafka.leave(&ns, group, &[b.str("MemberId").unwrap_or("").to_string()]);
            return Struct::new().with("ThrottleTimeMs", 0).with("ErrorCode", r.first().copied().unwrap_or(0));
        }
        let ids: Vec<String> = b.structs("Members").map(|m| m.str("MemberId").unwrap_or("").to_string()).collect();
        let r = self.shared.kafka.leave(&ns, group, &ids);
        let members: Vec<Struct> = b
            .structs("Members")
            .zip(r)
            .map(|(m, e)| {
                Struct::new()
                    .with("MemberId", m.str("MemberId").unwrap_or("").to_string())
                    .with("GroupInstanceId", m.str("GroupInstanceId").map(str::to_string))
                    .with("ErrorCode", e)
            })
            .collect();
        Struct::new().with("ThrottleTimeMs", 0).with("ErrorCode", 0).with("Members", members)
    }

    fn offset_commit(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let group = b.str("GroupId").unwrap_or("");
        let mut results = Vec::new();
        let mut todo: Vec<(String, i32, Committed)> = Vec::new();
        let mut shape: Vec<(String, Vec<(i32, i16)>)> = Vec::new();
        {
            let st = self.shared.state.lock();
            for t in b.structs("Topics") {
                let name = t.str("Name").unwrap_or("").to_string();
                let hub = st.hub(&ns, &name);
                let mut ps = Vec::new();
                for p in t.structs("Partitions") {
                    let idx = p.i32("PartitionIndex");
                    let md = p.str("CommittedMetadata").map(str::to_string);
                    let code = if hub.is_none_or(|h| idx < 0 || idx as usize >= h.partitions.len()) {
                        err::UNKNOWN_TOPIC_OR_PARTITION
                    } else if md.as_ref().is_some_and(|m| m.len() > MAX_OFFSET_METADATA_BYTES) {
                        err::OFFSET_METADATA_TOO_LARGE
                    } else if p.i64("CommittedOffset") < 0 {
                        err::INVALID_COMMIT_OFFSET_SIZE
                    } else {
                        todo.push((
                            hub.map(|h| h.name.clone()).unwrap_or_default(),
                            idx,
                            Committed { offset: p.i64("CommittedOffset"), metadata: md, leader_epoch: p.i32("CommittedLeaderEpoch") },
                        ));
                        err::NONE
                    };
                    ps.push((idx, code));
                }
                shape.push((name, ps));
            }
        }
        let group_err = if todo.is_empty() && !shape.is_empty() {
            // nothing valid to store; still check the group state so that a stale member is told
            self.shared.kafka.commit(&ns, group, b.i32("GenerationIdOrMemberEpoch"), b.str("MemberId").unwrap_or(""), Vec::new())
        } else {
            self.shared.kafka.commit(&ns, group, b.i32("GenerationIdOrMemberEpoch"), b.str("MemberId").unwrap_or(""), todo)
        };
        for (name, ps) in shape {
            let parts: Vec<Struct> = ps
                .into_iter()
                .map(|(idx, code)| {
                    Struct::new().with("PartitionIndex", idx).with("ErrorCode", if group_err != 0 { group_err } else { code })
                })
                .collect();
            results.push(Struct::new().with("Name", name).with("Partitions", parts));
        }
        Struct::new().with("ThrottleTimeMs", 0).with("Topics", results)
    }

    fn offset_fetch(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let group = b.str("GroupId").unwrap_or("");
        let mut topics = Vec::new();
        let part = |idx: i32, c: Option<Committed>| {
            let c = c.unwrap_or(Committed { offset: -1, metadata: None, leader_epoch: -1 });
            Struct::new()
                .with("PartitionIndex", idx)
                .with("CommittedOffset", c.offset)
                .with("CommittedLeaderEpoch", c.leader_epoch)
                .with("Metadata", c.metadata.unwrap_or_default())
                .with("ErrorCode", 0)
        };
        match b.get("Topics") {
            Value::Null => {
                let mut by_topic: Vec<(String, Vec<Struct>)> = Vec::new();
                for (t, p, cm) in self.shared.kafka.all_offsets(&ns, group) {
                    match by_topic.last_mut() {
                        Some((n, v)) if *n == t => v.push(part(p, Some(cm))),
                        _ => by_topic.push((t, vec![part(p, Some(cm))])),
                    }
                }
                for (t, ps) in by_topic {
                    topics.push(Struct::new().with("Name", t).with("Partitions", ps));
                }
            }
            req => {
                for t in req.structs() {
                    let name = t.str("Name").unwrap_or("").to_string();
                    let ps: Vec<Struct> = t
                        .items("PartitionIndexes")
                        .iter()
                        .map(|i| i.as_i32())
                        .map(|i| part(i, self.shared.kafka.fetch_offset(&ns, group, &name, i)))
                        .collect();
                    topics.push(Struct::new().with("Name", name).with("Partitions", ps));
                }
            }
        }
        let code = if group.is_empty() || group.len() > MAX_GROUP_ID_BYTES { err::INVALID_GROUP_ID } else { 0 };
        Struct::new().with("ThrottleTimeMs", 0).with("Topics", topics).with("ErrorCode", code)
    }

    fn describe_groups(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let mut out = Vec::new();
        for g in b.get("Groups").items() {
            let id = g.as_str().unwrap_or("").to_string();
            if id.is_empty() || id.len() > MAX_GROUP_ID_BYTES {
                out.push(
                    Struct::new()
                        .with("ErrorCode", err::INVALID_GROUP_ID)
                        .with("GroupId", id)
                        .with("GroupState", "Dead")
                        .with("ProtocolType", "")
                        .with("ProtocolData", "")
                        .with("Members", Vec::<Struct>::new()),
                );
                continue;
            }
            match self.shared.kafka.describe(&ns, &id) {
                None => out.push(
                    Struct::new()
                        .with("ErrorCode", 0)
                        .with("GroupId", id)
                        .with("GroupState", "Dead")
                        .with("ProtocolType", "")
                        .with("ProtocolData", "")
                        .with("Members", Vec::<Struct>::new()),
                ),
                Some(g) => {
                    let members: Vec<Struct> = g
                        .members
                        .into_iter()
                        .map(|m| {
                            Struct::new()
                                .with("MemberId", m.member_id)
                                .with("GroupInstanceId", m.instance_id)
                                .with("ClientId", m.client_id)
                                .with("ClientHost", m.client_host)
                                .with("MemberMetadata", m.metadata)
                                .with("MemberAssignment", m.assignment)
                        })
                        .collect();
                    out.push(
                        Struct::new()
                            .with("ErrorCode", 0)
                            .with("GroupId", g.group)
                            .with("GroupState", g.state)
                            .with("ProtocolType", g.protocol_type)
                            .with("ProtocolData", g.protocol)
                            .with("Members", members),
                    );
                }
            }
        }
        Struct::new().with("ThrottleTimeMs", 0).with("Groups", out)
    }

    fn list_groups(&self, c: &Conn, b: &Struct, version: i16) -> Struct {
        let filter: Vec<String> =
            if version >= 4 { b.items("StatesFilter").iter().filter_map(|s| s.as_str().map(str::to_string)).collect() } else { Vec::new() };
        let groups: Vec<Struct> = self
            .shared
            .kafka
            .list(&c.namespace())
            .into_iter()
            .filter(|g| filter.is_empty() || filter.iter().any(|f| f.eq_ignore_ascii_case(g.state)))
            .map(|g| Struct::new().with("GroupId", g.group).with("ProtocolType", g.protocol_type).with("GroupState", g.state))
            .collect();
        Struct::new().with("ThrottleTimeMs", 0).with("ErrorCode", 0).with("Groups", groups)
    }

    // ---- topics ----------------------------------------------------------------------------------------------------

    fn create_topics(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let validate_only = b.bool("validateOnly");
        let now = now_ms();
        let mut results = Vec::new();
        for t in b.structs("Topics") {
            let name = t.str("Name").unwrap_or("").to_string();
            let mut parts = t.i32("NumPartitions");
            let mut retention_ms = DEFAULT_RETENTION_MS;
            let mut problem: Option<(i16, String)> = None;
            if !valid_topic_name(&name) {
                problem = Some((
                    err::INVALID_TOPIC_EXCEPTION,
                    format!("Topic name '{name}' is invalid: use letters, digits, '.', '_' and '-', at most 249 characters"),
                ));
            } else if parts == 0 || parts < -1 {
                problem = Some((err::INVALID_PARTITIONS, "Number of partitions must be larger than 0.".to_string()));
            } else if t.i64("ReplicationFactor") == 0 || t.i64("ReplicationFactor") < -1 {
                problem = Some((err::INVALID_REPLICATION_FACTOR, "Replication factor must be larger than 0.".to_string()));
            } else if !t.items("Assignments").is_empty() {
                problem = Some((
                    err::INVALID_REQUEST,
                    "Manual replica assignment is not supported: partitions are those of the event hub.".to_string(),
                ));
            }
            if parts == -1 {
                parts = DEFAULT_PARTITIONS as i32;
            }
            if problem.is_none() {
                for cfg in t.structs("Configs") {
                    let (k, val) = (cfg.str("Name").unwrap_or(""), cfg.str("Value"));
                    match k {
                        "cleanup.policy" if val.is_some_and(|v| v.contains("compact")) => {
                            problem = Some((err::INVALID_CONFIG, "Log compaction is not built (cleanup.policy=compact)".to_string()));
                        }
                        "retention.ms" => match val.and_then(|v| v.trim().parse::<i64>().ok()).filter(|v| *v > 0) {
                            Some(ms) => retention_ms = ms,
                            None => {
                                problem = Some((
                                    err::INVALID_CONFIG,
                                    "retention.ms must be a positive number of milliseconds (infinite retention is not built)".to_string(),
                                ))
                            }
                        },
                        _ => {}
                    }
                }
            }
            if problem.is_none() && !validate_only {
                let mut st = self.shared.state.lock();
                if let Err(e) = st.create_hub(&ns, &name, parts as u32, retention_ms, now) {
                    problem = Some(if e.condition == crate::errors::ENTITY_ALREADY_EXISTS {
                        (err::TOPIC_ALREADY_EXISTS, format!("Topic '{name}' already exists."))
                    } else if e.condition == crate::errors::ARGUMENT_OUT_OF_RANGE && e.description.contains("PartitionCount") {
                        (err::INVALID_PARTITIONS, e.description)
                    } else {
                        (err::INVALID_REQUEST, e.description)
                    });
                }
            } else if problem.is_none() && validate_only && self.shared.state.lock().hub(&ns, &name).is_some() {
                problem = Some((err::TOPIC_ALREADY_EXISTS, format!("Topic '{name}' already exists.")));
            }
            let ok = problem.is_none();
            let configs: Vec<Struct> = [
                ("retention.ms", retention_ms.to_string()),
                ("cleanup.policy", "delete".to_string()),
                ("max.message.bytes", MAX_MESSAGE_SIZE.to_string()),
            ]
            .into_iter()
            .map(|(k, v)| {
                Struct::new().with("Name", k).with("Value", v).with("ReadOnly", false).with("ConfigSource", 5).with("IsSensitive", false)
            })
            .collect();
            results.push(
                Struct::new()
                    .with("Name", name)
                    .with("TopicId", Value::Uuid([0; 16]))
                    .with("ErrorCode", problem.as_ref().map(|p| p.0).unwrap_or(0))
                    .with("ErrorMessage", problem.map(|p| Value::Str(p.1)).unwrap_or(Value::Null))
                    .with("NumPartitions", if ok { parts } else { -1 })
                    .with("ReplicationFactor", if ok { 1 } else { -1 })
                    .with("Configs", if ok { Value::Array(configs.into_iter().map(Value::Struct).collect()) } else { Value::Null }),
            );
        }
        Struct::new().with("ThrottleTimeMs", 0).with("Topics", results)
    }

    fn delete_topics(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let mut results = Vec::new();
        for n in b.get("TopicNames").items() {
            let name = n.as_str().unwrap_or("").to_string();
            let deleted = self.shared.delete_hub(&ns, &name);
            results.push(
                Struct::new().with("Name", name).with("ErrorCode", if deleted { 0 } else { err::UNKNOWN_TOPIC_OR_PARTITION }).with(
                    "ErrorMessage",
                    if deleted { Value::Null } else { Value::Str("This server does not host this topic-partition.".into()) },
                ),
            );
        }
        Struct::new().with("ThrottleTimeMs", 0).with("Responses", results)
    }

    fn create_partitions(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let st = self.shared.state.lock();
        let results: Vec<Struct> = b
            .structs("Topics")
            .map(|t| {
                let name = t.str("Name").unwrap_or("").to_string();
                let (code, msg) = match st.hub(&ns, &name) {
                    None => (err::UNKNOWN_TOPIC_OR_PARTITION, "This server does not host this topic-partition.".to_string()),
                    Some(h) if t.i32("Count") as usize == h.partitions.len() => {
                        (err::INVALID_PARTITIONS, format!("Topic already has {} partitions.", h.partitions.len()))
                    }
                    Some(_) => (
                        err::INVALID_PARTITIONS,
                        "The partition count of an event hub cannot be changed after it was created (not built).".to_string(),
                    ),
                };
                Struct::new().with("Name", name).with("ErrorCode", code).with("ErrorMessage", msg)
            })
            .collect();
        Struct::new().with("ThrottleTimeMs", 0).with("Results", results)
    }

    fn describe_configs(&self, c: &Conn, b: &Struct) -> Struct {
        let ns = c.namespace();
        let st = self.shared.state.lock();
        let mut results = Vec::new();
        for r in b.structs("Resources") {
            let rtype = r.i16("ResourceType");
            let name = r.str("ResourceName").unwrap_or("").to_string();
            let wanted: Option<Vec<String>> = match r.get("ConfigurationKeys") {
                Value::Null => None,
                v => Some(v.items().iter().filter_map(|k| k.as_str().map(str::to_string)).collect()),
            };
            // (name, value, config type: 1 boolean, 2 string, 3 int, 5 long, 7 list)
            let (code, msg, entries): ConfigAnswer = match rtype {
                2 => match st.hub(&ns, &name) {
                    None => (err::UNKNOWN_TOPIC_OR_PARTITION, Some("This server does not host this topic-partition.".into()), vec![]),
                    Some(h) => (
                        0,
                        None,
                        vec![
                            ("cleanup.policy", "delete".into(), 7),
                            ("retention.ms", h.retention_ms.to_string(), 5),
                            ("max.message.bytes", MAX_MESSAGE_SIZE.to_string(), 3),
                            ("message.timestamp.type", "CreateTime".into(), 2),
                            ("compression.type", "producer".into(), 2),
                            ("min.insync.replicas", "1".into(), 3),
                        ],
                    ),
                },
                4 => (
                    0,
                    None,
                    vec![
                        ("auto.create.topics.enable", self.shared.autocreate.to_string(), 1),
                        ("num.partitions", DEFAULT_PARTITIONS.to_string(), 3),
                        ("message.max.bytes", MAX_MESSAGE_SIZE.to_string(), 3),
                        ("group.min.session.timeout.ms", super::groups::MIN_SESSION_TIMEOUT_MS.to_string(), 3),
                        ("group.max.session.timeout.ms", super::groups::MAX_SESSION_TIMEOUT_MS.to_string(), 3),
                    ],
                ),
                _ => (err::INVALID_REQUEST, Some(format!("resource type {rtype} is not built")), vec![]),
            };
            let configs: Vec<Struct> = entries
                .into_iter()
                .filter(|(k, _, _)| wanted.as_ref().is_none_or(|w| w.iter().any(|x| x == k)))
                .map(|(k, v, ty)| {
                    Struct::new()
                        .with("Name", k)
                        .with("Value", v)
                        .with("ReadOnly", false)
                        .with("IsDefault", true)
                        .with("ConfigSource", 5)
                        .with("IsSensitive", false)
                        .with("Synonyms", Vec::<Struct>::new())
                        .with("ConfigType", ty)
                        .with("Documentation", Value::Null)
                })
                .collect();
            results.push(
                Struct::new()
                    .with("ErrorCode", code)
                    .with("ErrorMessage", msg)
                    .with("ResourceType", rtype)
                    .with("ResourceName", name)
                    .with("Configs", configs),
            );
        }
        Struct::new().with("ThrottleTimeMs", 0).with("Results", results)
    }
}

/// (error code, message, [(config name, value, config type)])
type ConfigAnswer<'a> = (i16, Option<String>, Vec<(&'a str, String, i8)>);

fn topic_error(name: &str, code: i16) -> Struct {
    Struct::new().with("ErrorCode", code).with("Name", name.to_string()).with("IsInternal", false).with("Partitions", Vec::<Struct>::new())
}

/// The host of `Endpoint=sb://host[:port]/` inside a connection string.
pub fn endpoint_host(conn_str: &str) -> Option<String> {
    for part in conn_str.split(';') {
        let part = part.trim();
        // `get` is None when byte 9 is inside a multi-byte character: the text is then not "endpoint=" anyway
        if part.len() > 9 && part.get(..9).is_some_and(|p| p.eq_ignore_ascii_case("endpoint=")) {
            let v = &part[9..];
            let v = v.split_once("://").map(|(_, r)| r).unwrap_or(v);
            let host = v.split(['/', '?']).next().unwrap_or("");
            // drop a port
            let host = match host.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => h,
                _ => host,
            };
            return Some(host.to_string()).filter(|h| !h.is_empty());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_host_from_connection_string() {
        assert_eq!(
            endpoint_host("Endpoint=sb://myns.servicebus.windows.net/;SharedAccessKeyName=k;SharedAccessKey=x").as_deref(),
            Some("myns.servicebus.windows.net")
        );
        assert_eq!(endpoint_host("SharedAccessKeyName=k;Endpoint=sb://localhost:5672;SharedAccessKey=x").as_deref(), Some("localhost"));
        assert_eq!(endpoint_host("endpoint=sb://ns.eventhub.localhost:9093/;x=y").as_deref(), Some("ns.eventhub.localhost"));
        assert_eq!(endpoint_host("not a connection string"), None);
    }

    #[test]
    fn endpoint_host_never_panics_on_text_with_multi_byte_characters() {
        // the review's password, and every offset of a multi-byte character against the 9 byte prefix
        assert_eq!(endpoint_host("éééééx"), None);
        for lead in 0..12 {
            for ch in ["é", "日", "😀", "ǆ"] {
                let s = format!("{}{ch}{}", "a".repeat(lead), "endpoint=sb://h/");
                let _ = endpoint_host(&s);
                let _ = endpoint_host(&format!("x=y;{s}"));
                let _ = endpoint_host(&format!("{ch}{ch}{ch}{ch}{ch}x"));
            }
        }
        assert_eq!(endpoint_host("é=1;Endpoint=sb://h.example/;x=é").as_deref(), Some("h.example"));
    }

    #[test]
    fn topic_names() {
        assert!(valid_topic_name("orders.v1_a-b"));
        assert!(!valid_topic_name(""));
        assert!(!valid_topic_name(".."));
        assert!(!valid_topic_name("a/b"));
        assert!(!valid_topic_name(&"a".repeat(250)));
    }
}
