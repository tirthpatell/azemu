//! Strict authorisation for Queue Storage (`docs/strict-auth.md`): the data action an operation needs, from Microsoft's "Permissions for calling
//! data operations" and from what Azure did in November 2026 (a Data Reader peeks and lists, and may not receive).

use azemu_core::auth::{Auth, Need, Resource};
use azemu_core::storage::strict::{self, Kind, Refusal};
use azemu_core::{Request, Response};

use crate::routing::{Op, Routed};

const SVC: &str = "Microsoft.Storage/storageAccounts/queueServices";
const QUEUES: &str = "Microsoft.Storage/storageAccounts/queueServices/queues";
const MESSAGES: &str = "Microsoft.Storage/storageAccounts/queueServices/queues/messages";

fn queues(a: &str) -> Need {
    Need::control(&format!("{QUEUES}/{a}"))
}
fn messages(a: &str) -> Need {
    Need::data(&format!("{MESSAGES}/{a}"))
}

pub fn need_of(routed: &Routed, account: &str) -> (Need, Resource, Refusal) {
    let on_account = Resource::storage(account);
    let on_queue = Resource::queue(account, routed.queue.as_deref().unwrap_or(""));
    let m = Refusal::Mismatch;
    match routed.op {
        Op::ListQueues => (queues("read"), on_account, m),
        Op::GetServiceProperties | Op::GetServiceStats => (Need::control(&format!("{SVC}/read")), on_account, m),
        // Microsoft's table says `read` here, which is its typo for `write`: a Data Reader (and a Reader) was refused
        Op::SetServiceProperties => (Need::control(&format!("{SVC}/write")), on_account, m),
        Op::GetUserDelegationKey => (Need::control(&format!("{SVC}/generateUserDelegationKey/action")), on_account, m),
        Op::CreateQueue => (queues("write"), on_queue, Refusal::Failure),
        Op::DeleteQueue => (queues("delete"), on_queue, m),
        Op::GetQueueMetadata => (queues("read"), on_queue, m),
        Op::SetQueueMetadata => (queues("write"), on_queue, m),
        Op::GetQueueAcl => (queues("getAcl/action"), on_queue, m),
        Op::SetQueueAcl => (queues("setAcl/action"), on_queue, m),
        Op::PutMessage => (Need::any(vec![messages("add/action"), messages("write")]), on_queue, m),
        Op::GetMessages => {
            (Need::any(vec![messages("process/action"), Need::all(vec![messages("delete"), messages("read")])]), on_queue, m)
        }
        Op::PeekMessages => (messages("read"), on_queue, m),
        Op::ClearMessages => (messages("delete"), on_queue, m),
        Op::UpdateMessage => (messages("write"), on_queue, m),
        Op::DeleteMessage => (Need::any(vec![messages("process/action"), messages("delete")]), on_queue, m),
    }
}

/// `Some(answer)` when the request is refused.
pub fn gate(auth: &Auth, req: &Request, routed: &Routed) -> Option<Response> {
    if !auth.applies(req) {
        return None;
    }
    let account = req.account.to_ascii_lowercase();
    let who = match strict::authenticate(auth, req, Kind::Queue, &account) {
        Ok(w) => w,
        Err(r) => return Some(r),
    };
    let (need, res, which) = need_of(routed, &account);
    strict::authorize(auth, req, Kind::Queue, &who, &need, &res, which)
}
