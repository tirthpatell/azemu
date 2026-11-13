//! Which response link answers a request on a request/response node (`$management`, `$cbs`).
//!
//! A client opens a pair of links to the node: a sender for the requests and a receiver for the
//! responses. The receiver's `target` address is the "reply address" and a request names it in
//! its `reply-to` property (Java and Go do this; the Python SDK sends no `reply-to` at all).
//! Several pairs can share one connection (the Python SDK opens one pair per sender and per
//! receiver, all to `<entity>/$management`), so a response must not go to every response link:
//! the others would get answers to requests they never made.

use std::collections::HashMap;

use crate::api::LinkId;

/// Pairs the request links and response links of one node by the order they attach in, within
/// one session. This only breaks ties: it decides when a session holds several pairs and
/// nothing else (a `reply-to`) tells them apart.
#[derive(Default)]
pub struct Pairing {
    requests: Vec<(u16, LinkId)>,
    responses: Vec<(u16, LinkId)>,
    /// request link to response link
    pairs: HashMap<LinkId, LinkId>,
}

impl Pairing {
    pub fn add_request(&mut self, session: u16, link: LinkId) {
        if let Some(i) = self.responses.iter().position(|(s, _)| *s == session) {
            let (_, resp) = self.responses.remove(i);
            self.pairs.insert(link, resp);
        } else {
            self.requests.push((session, link));
        }
    }

    pub fn add_response(&mut self, session: u16, link: LinkId) {
        if let Some(i) = self.requests.iter().position(|(s, _)| *s == session) {
            let (_, req) = self.requests.remove(i);
            self.pairs.insert(req, link);
        } else {
            self.responses.push((session, link));
        }
    }

    pub fn response_for(&self, request: LinkId) -> Option<LinkId> {
        self.pairs.get(&request).copied()
    }

    /// A link of the node ended.
    pub fn remove(&mut self, link: LinkId) {
        self.pairs.retain(|req, resp| *req != link && *resp != link);
        self.requests.retain(|(_, l)| *l != link);
        self.responses.retain(|(_, l)| *l != link);
    }
}

/// A response link of the node, as the service saw it attach.
#[derive(Debug, Clone)]
pub struct ResponseLink {
    pub link: LinkId,
    /// The session (channel) the link was attached on.
    pub session: u16,
    /// The `target` address of the peer's receiving end.
    pub address: Option<String>,
}

/// The response links that answer a request that arrived on a link of `request_session` with
/// the given `reply-to`. In order of preference:
///
/// 1. links whose address is the `reply-to`; when some of those are on the request's session,
///    only those;
/// 2. links on the request's session (the pair the request link belongs to);
/// 3. every link of the node (a client that pairs its links some other way).
///
/// When that leaves more than one link and `paired` (the response link that attached with the
/// request link, see [`Pairing`]) is one of them, only `paired` answers.
/// The result is empty only when there is no response link at all.
pub fn pick(request_session: u16, reply_to: Option<&str>, paired: Option<LinkId>, links: &[ResponseLink]) -> Vec<LinkId> {
    let same_session = |l: &&ResponseLink| l.session == request_session;
    let mut chosen: Vec<LinkId> = Vec::new();
    if let Some(r) = reply_to.filter(|r| !r.is_empty()) {
        let named: Vec<&ResponseLink> = links.iter().filter(|l| l.address.as_deref() == Some(r)).collect();
        if !named.is_empty() {
            let mine: Vec<&ResponseLink> = named.iter().copied().filter(same_session).collect();
            let picked = if mine.is_empty() { named } else { mine };
            chosen = picked.iter().map(|l| l.link).collect();
        }
    }
    if chosen.is_empty() {
        chosen = links.iter().filter(same_session).map(|l| l.link).collect();
    }
    if chosen.is_empty() {
        chosen = links.iter().map(|l| l.link).collect();
    }
    if chosen.len() > 1 {
        if let Some(p) = paired.filter(|p| chosen.contains(p)) {
            return vec![p];
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(link: LinkId, session: u16, address: Option<&str>) -> ResponseLink {
        ResponseLink { link, session, address: address.map(str::to_string) }
    }

    #[test]
    fn the_reply_to_address_picks_the_link() {
        let links = [l(1, 0, Some("a-reply")), l(2, 0, Some("b-reply"))];
        assert_eq!(pick(0, Some("b-reply"), None, &links), vec![2]);
        assert_eq!(pick(0, Some("a-reply"), None, &links), vec![1]);
    }

    #[test]
    fn without_a_reply_to_the_pair_in_the_same_session_answers() {
        let links = [l(1, 0, Some("q/$management")), l(2, 1, Some("q/$management")), l(3, 2, Some("q/$management"))];
        assert_eq!(pick(1, None, None, &links), vec![2]);
        assert_eq!(pick(2, Some(""), None, &links), vec![3]);
    }

    #[test]
    fn an_unknown_reply_to_falls_back_to_the_session() {
        let links = [l(1, 0, None), l(2, 1, None)];
        assert_eq!(pick(1, Some("nobody"), None, &links), vec![2]);
    }

    #[test]
    fn the_same_address_on_two_sessions_prefers_the_requests_session() {
        let links = [l(1, 0, Some("r")), l(2, 1, Some("r"))];
        assert_eq!(pick(1, Some("r"), None, &links), vec![2]);
        // none on the request's session: every link with that address
        assert_eq!(pick(5, Some("r"), None, &links), vec![1, 2]);
    }

    #[test]
    fn two_pairs_on_one_session_are_told_apart_by_attach_order() {
        let mut p = Pairing::default();
        // response, request, response, request (what the SDKs do), all on session 0
        p.add_response(0, 1);
        p.add_request(0, 2);
        p.add_response(0, 3);
        p.add_request(0, 4);
        let links = [l(1, 0, None), l(3, 0, None)];
        assert_eq!(pick(0, None, p.response_for(2), &links), vec![1]);
        assert_eq!(pick(0, None, p.response_for(4), &links), vec![3]);
        p.remove(1);
        p.remove(2);
        assert_eq!(p.response_for(2), None);
        assert_eq!(p.response_for(4), Some(3));
    }

    #[test]
    fn a_pair_on_another_session_is_not_paired() {
        let mut p = Pairing::default();
        p.add_response(0, 1);
        p.add_request(1, 2);
        assert_eq!(p.response_for(2), None);
    }

    #[test]
    fn a_client_with_its_links_on_different_sessions_still_gets_an_answer() {
        let links = [l(1, 3, None), l(2, 3, None)];
        assert_eq!(pick(0, None, None, &links), vec![1, 2]);
        assert!(pick(0, None, None, &[]).is_empty());
    }
}
