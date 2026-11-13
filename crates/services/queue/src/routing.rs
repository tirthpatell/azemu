//! Turn method + path + query into one operation.

use azemu_core::http::Method;
use azemu_core::Request;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    ListQueues,
    GetServiceProperties,
    SetServiceProperties,
    GetServiceStats,
    GetUserDelegationKey,
    CreateQueue,
    DeleteQueue,
    GetQueueMetadata,
    SetQueueMetadata,
    GetQueueAcl,
    SetQueueAcl,
    PutMessage,
    GetMessages,
    PeekMessages,
    ClearMessages,
    UpdateMessage,
    DeleteMessage,
}

/// An operation and what it is aimed at.
#[derive(Debug, Clone)]
pub struct Routed {
    pub op: Op,
    pub queue: Option<String>,
    pub message_id: Option<String>,
}

/// Why a request did not become an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrouted {
    /// The resource exists but not for this HTTP verb.
    UnsupportedVerb,
    /// The URL is not a queue URL at all.
    BadUri,
    /// `comp=` (or `restype=`) names something the Queue API does not have.
    BadQuery(&'static str, String),
}

pub fn route(req: &Request) -> Result<Routed, Unrouted> {
    let segs = req.segments();
    let m = &req.method;
    let comp = req.query.get("comp");
    let get = *m == Method::GET || *m == Method::HEAD;

    // `OPTIONS` (CORS preflight) is answered before routing, see `QueueService::handle`.

    let routed = |op: Op, queue: Option<&String>, message_id: Option<&String>| {
        Ok(Routed { op, queue: queue.cloned(), message_id: message_id.cloned() })
    };

    match segs.as_slice() {
        [] => {
            let restype = req.query.get("restype");
            match (comp, restype) {
                (Some("list"), None) | (Some("list"), Some("")) => {
                    if get {
                        routed(Op::ListQueues, None, None)
                    } else {
                        Err(Unrouted::UnsupportedVerb)
                    }
                }
                (Some("properties"), Some("service")) => {
                    if get {
                        routed(Op::GetServiceProperties, None, None)
                    } else if *m == Method::PUT {
                        routed(Op::SetServiceProperties, None, None)
                    } else {
                        Err(Unrouted::UnsupportedVerb)
                    }
                }
                (Some("stats"), Some("service")) => {
                    if get {
                        routed(Op::GetServiceStats, None, None)
                    } else {
                        Err(Unrouted::UnsupportedVerb)
                    }
                }
                (Some("userdelegationkey"), Some("service")) => {
                    if *m == Method::POST {
                        routed(Op::GetUserDelegationKey, None, None)
                    } else {
                        Err(Unrouted::UnsupportedVerb)
                    }
                }
                (Some(c), _) => Err(Unrouted::BadQuery("comp", c.to_string())),
                (None, _) => Err(Unrouted::UnsupportedVerb),
            }
        }
        [q] => match comp {
            None => {
                if *m == Method::PUT {
                    routed(Op::CreateQueue, Some(q), None)
                } else if *m == Method::DELETE {
                    routed(Op::DeleteQueue, Some(q), None)
                } else {
                    Err(Unrouted::UnsupportedVerb)
                }
            }
            Some("metadata") => {
                if get {
                    routed(Op::GetQueueMetadata, Some(q), None)
                } else if *m == Method::PUT {
                    routed(Op::SetQueueMetadata, Some(q), None)
                } else {
                    Err(Unrouted::UnsupportedVerb)
                }
            }
            Some("acl") => {
                if get {
                    routed(Op::GetQueueAcl, Some(q), None)
                } else if *m == Method::PUT {
                    routed(Op::SetQueueAcl, Some(q), None)
                } else {
                    Err(Unrouted::UnsupportedVerb)
                }
            }
            Some(c) => Err(Unrouted::BadQuery("comp", c.to_string())),
        },
        [q, messages] if messages == "messages" => {
            if *m == Method::GET {
                match req.query.get("peekonly") {
                    None => routed(Op::GetMessages, Some(q), None),
                    Some(v) if v.eq_ignore_ascii_case("true") => routed(Op::PeekMessages, Some(q), None),
                    Some(v) if v.eq_ignore_ascii_case("false") => routed(Op::GetMessages, Some(q), None),
                    Some(v) => Err(Unrouted::BadQuery("peekonly", v.to_string())),
                }
            } else if *m == Method::POST {
                routed(Op::PutMessage, Some(q), None)
            } else if *m == Method::DELETE {
                routed(Op::ClearMessages, Some(q), None)
            } else {
                Err(Unrouted::UnsupportedVerb)
            }
        }
        [q, messages, id] if messages == "messages" => {
            if *m == Method::PUT {
                routed(Op::UpdateMessage, Some(q), Some(id))
            } else if *m == Method::DELETE {
                routed(Op::DeleteMessage, Some(q), Some(id))
            } else {
                Err(Unrouted::UnsupportedVerb)
            }
        }
        _ => Err(Unrouted::BadUri),
    }
}
