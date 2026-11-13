//! Offers: provisioned throughput of databases and containers (manual and autoscale).

use serde_json::{json, Value};

use crate::errors::{CosmosError, R};
use crate::out::{Cx, Out};
use crate::query;
use crate::rid;
use crate::state::{Account, Meta, Offer};

pub enum Throughput {
    Manual(u64),
    Auto(Value),
}

fn min_msg(requested: i64, min: u64) -> CosmosError {
    CosmosError::bad_request(format!("Requested throughput {requested} is less than required minimum throughput {min}."))
}

fn auto_max(settings: &Value) -> i64 {
    settings.get("maxThroughput").and_then(Value::as_i64).unwrap_or(0)
}

fn check_auto(settings: &Value) -> R<()> {
    let max = auto_max(settings);
    if max < 1000 {
        return Err(min_msg(max, 1000));
    }
    if max % 1000 != 0 {
        return Err(CosmosError::bad_request(format!("Requested autoscale max throughput {max} is not a multiple of 1000.")));
    }
    Ok(())
}

fn check_manual(n: i64) -> R<u64> {
    if n < 400 {
        return Err(min_msg(n, 400));
    }
    if n % 100 != 0 {
        return Err(CosmosError::bad_request(format!("Requested throughput {n} is not a multiple of 100.")));
    }
    Ok(n as u64)
}

/// Throughput asked for by the creation headers, if any.
pub fn from_headers(cx: &Cx) -> R<Option<Throughput>> {
    if let Some(a) = cx.header("x-ms-cosmos-offer-autopilot-settings") {
        let v: Value = serde_json::from_str(a).map_err(|_| CosmosError::bad_request("The autopilot settings are not valid JSON."))?;
        check_auto(&v)?;
        return Ok(Some(Throughput::Auto(v)));
    }
    if let Some(t) = cx.header("x-ms-offer-throughput") {
        let n: i64 = t.trim().parse().map_err(|_| CosmosError::bad_request("x-ms-offer-throughput must be an integer."))?;
        return Ok(Some(Throughput::Manual(check_manual(n)?)));
    }
    Ok(None)
}

fn content_for(tp: &Throughput) -> Value {
    match tp {
        Throughput::Manual(n) => json!({
            "offerThroughput": n,
            "offerIsRUPerMinuteThroughputEnabled": false
        }),
        Throughput::Auto(settings) => {
            let mut s = settings.clone();
            if let Some(o) = s.as_object_mut() {
                let max = o.get("maxThroughput").cloned().unwrap_or(json!(1000));
                o.remove("tier");
                let mut m = serde_json::Map::new();
                m.insert("tier".into(), json!(0));
                m.insert("maxThroughput".into(), max);
                for (k, v) in o.iter() {
                    if k != "maxThroughput" {
                        m.insert(k.clone(), v.clone());
                    }
                }
                s = Value::Object(m);
            }
            json!({
                "offerThroughput": auto_max(&s) / 10,
                "offerIsRUPerMinuteThroughputEnabled": false,
                "offerAutopilotSettings": s
            })
        }
    }
}

/// Create an offer for a resource; returns the offer rid.
pub fn create(acct: &mut Account, now: i64, tp: &Throughput, resource: String, resource_rid: String) -> String {
    let (bytes, used) = rid::offer(acct.offer_seq + 1);
    acct.offer_seq = used;
    let seq = acct.next_seq();
    let meta = Meta::new("", bytes, seq, now);
    let rid = meta.rid.clone();
    acct.offers.insert(rid.clone(), Offer { meta, resource, resource_rid, content: content_for(tp) });
    rid
}

pub fn remove(acct: &mut Account, offer: &Option<String>) {
    if let Some(r) = offer {
        acct.offers.remove(r);
    }
}

fn find<'a>(acct: &'a mut Account, key: &str) -> R<&'a mut Offer> {
    let k = acct.offers.keys().find(|r| rid::matches(r, key)).cloned().ok_or_else(CosmosError::not_found_default)?;
    acct.offers.get_mut(&k).ok_or_else(CosmosError::not_found_default)
}

pub fn list_or_query(cx: &Cx, acct: &Account) -> R<Out> {
    let mut items: Vec<_> = acct.offers.values().collect();
    items.sort_by_key(|o| o.meta.seq);
    query::feed(cx, "", "Offers", items.iter().map(|o| o.to_json()).collect())
}

pub fn read(acct: &mut Account, key: &str) -> R<Out> {
    let o = find(acct, key)?;
    let v = o.to_json();
    Ok(crate::resources::resource_out(200, v))
}

pub fn replace(cx: &Cx, acct: &mut Account, key: &str) -> R<Out> {
    let body = cx.body_object()?;
    let o = find(acct, key)?;
    let content =
        body.get("content").and_then(Value::as_object).ok_or_else(|| CosmosError::bad_request("The offer content is missing."))?;
    let tp = if let Some(a) = content.get("offerAutopilotSettings") {
        check_auto(a)?;
        Throughput::Auto(a.clone())
    } else {
        let n = content
            .get("offerThroughput")
            .and_then(Value::as_i64)
            .ok_or_else(|| CosmosError::bad_request("offerThroughput is missing."))?;
        Throughput::Manual(check_manual(n)?)
    };
    o.content = content_for(&tp);
    o.meta.touch(cx.now);
    let v = o.to_json();
    Ok(crate::resources::resource_out(200, v))
}
