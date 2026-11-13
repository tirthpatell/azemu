//! Which requests core drops when their client goes away. The end-to-end proof for a stored procedure run is
//! `script_workers.rs` (`a_client_that_goes_away_mid_script_has_its_worker_ended_by_the_time_limit_at_the_latest`).

use azemu_core::http::Method;
use azemu_core::Service;
use azemu_cosmos::CosmosService;

#[test]
fn only_a_stored_procedure_run_asks_core_to_drop_it() {
    let s = CosmosService::new();
    let yes = [
        (Method::POST, "/dbs/d/colls/c/sprocs/spin"),
        (Method::POST, "/dbs/d/colls/c/sprocs/spin/"),
        (Method::POST, "/dbs/d/colls/c/sprocs/a%23b"),
    ];
    for (m, p) in yes {
        assert!(s.abort_on_disconnect(&m, p), "{m} {p}");
    }
    let no = [
        (Method::GET, "/dbs/d/colls/c/sprocs/spin"),
        (Method::PUT, "/dbs/d/colls/c/sprocs/spin"),
        (Method::DELETE, "/dbs/d/colls/c/sprocs/spin"),
        (Method::POST, "/dbs/d/colls/c/sprocs"),
        (Method::POST, "/dbs/d/colls/c/docs"),
        (Method::POST, "/dbs/d/colls/c/triggers"),
        (Method::POST, "/dbs/d/colls/c/udfs"),
        (Method::PATCH, "/dbs/d/colls/c/docs/x"),
        (Method::PUT, "/dbs/d/colls/c/docs/x"),
        (Method::DELETE, "/dbs/d/colls/c/docs/x"),
        (Method::POST, "/dbs"),
        (Method::POST, "/dbs/d/colls"),
        (Method::POST, "/dbs/d/colls/c/operations/partitionkeydelete"),
        (Method::POST, "/dbs/d/colls/c/sprocs/spin/extra"),
    ];
    for (m, p) in no {
        assert!(!s.abort_on_disconnect(&m, p), "{m} {p}");
    }
}
