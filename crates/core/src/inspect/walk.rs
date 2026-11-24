//! Tests only: walk the whole inspector tree of a running test server (`azemu_core::inspect::walk`).

use std::collections::{HashSet, VecDeque};

use serde_json::Value;

use crate::testing::TestServer;

/// One node the walk reached: the path it was asked at, and the first page of the answer.
pub struct Visited {
    pub requested: String,
    pub node: Value,
}

/// Visit every node under `start` (`""` is the root, `"/blob"` one service), following every `children[].path` and every `items[].path`
/// and every page (`?limit=` is `limit`, so a small number makes several pages). Asserts for each answer: status 200, the common
/// shape, that `path` is the path asked (without its query), and that paging ends. Returns the nodes in the order they were reached.
pub async fn walk(srv: &TestServer, start: &str, limit: usize) -> Vec<Visited> {
    let base = srv.gateway_url();
    let mut queue: VecDeque<String> = VecDeque::from([start.to_string()]);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        assert!(seen.len() < 20_000, "the walk does not end: more than 20000 nodes");
        let mut marker: Option<String> = None;
        let mut pages = 0;
        let mut first: Option<Value> = None;
        loop {
            let join = if path.contains('?') { '&' } else { '?' };
            let mut url = format!("{base}/_azemu/inspect{path}{join}limit={limit}");
            if let Some(m) = &marker {
                url.push_str(&format!("&marker={m}"));
            }
            let r = srv.send("GET", &url, &[], "").await;
            assert_eq!(r.status, 200, "GET {url}: {}", r.text());
            let v = r.json();
            for k in ["path", "kind", "title", "properties", "children", "items", "next"] {
                assert!(v.get(k).is_some(), "{url}: the answer has no '{k}'");
            }
            let asked = path.split('?').next().unwrap_or("");
            let asked = if asked.is_empty() { "/" } else { asked };
            assert_eq!(v["path"].as_str(), Some(asked), "{url}: the node's path is not the path asked");
            assert!(v["items"].as_array().is_some_and(|i| i.len() <= limit), "{url}: more rows than the limit");
            if first.is_none() {
                for c in v["children"].as_array().unwrap() {
                    queue.push_back(c["path"].as_str().unwrap_or_else(|| panic!("{url}: a child without a path")).to_string());
                }
            }
            for row in v["items"].as_array().unwrap() {
                if let Some(p) = row.get("path").and_then(Value::as_str) {
                    queue.push_back(p.to_string());
                }
            }
            marker = v["next"].as_str().map(str::to_string);
            if first.is_none() {
                first = Some(v);
            }
            pages += 1;
            assert!(pages <= 1000, "{url}: paging does not end");
            if marker.is_none() {
                break;
            }
        }
        out.push(Visited { requested: path, node: first.unwrap() });
    }
    out
}
