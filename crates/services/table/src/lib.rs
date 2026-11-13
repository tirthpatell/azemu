//! Azure Table Storage (OData v3 JSON REST API) for azemu.
//!
//! Mounted at `{account}.table.<anything>`; on a pinned port it also answers
//! Azurite-style `/{account}/{table}(...)` paths. Any account name works
//! without being created first.

mod acl;
mod api;
mod batch;
mod entity;
mod entity_api;
mod errors;
mod filter;
mod json;
mod multipart;
mod odata;
mod persist;
mod props;
mod routing;
mod sas;
mod service_api;
mod state;
mod table_api;
mod values;
mod xml;

use azemu_core::http::Method;
use azemu_core::storage::{finish, StorageAccounts};
use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::api::{check_name, Rq};
use crate::entity::Mode;
use crate::errors::{not_implemented, render, resource_not_found, ApiError, Res};
use crate::routing::Target;
use crate::state::State;

const SERVER: &str = "Windows-Azure-Table/1.0 Microsoft-HTTPAPI/2.0";

/// The Table Storage service.
#[derive(Default)]
pub struct TableService {
    state: Mutex<State>,
    accounts: StorageAccounts,
    /// Raised on every change to the state that a save would write (see `persist_generation`).
    generation: AtomicU64,
}

impl TableService {
    pub fn new() -> Self {
        Self::default()
    }

    /// The account's CORS rules in the form `azemu_core::storage::cors` takes.
    fn cors_rules(&self, account: &str) -> Vec<azemu_core::storage::cors::CorsRule> {
        let st = self.state.lock();
        let Some(acct) = st.accounts.get(account) else { return Vec::new() };
        acct.props
            .cors
            .iter()
            .map(|r| azemu_core::storage::cors::CorsRule {
                allowed_origins: r.allowed_origins.clone(),
                allowed_methods: r.allowed_methods.clone(),
                allowed_headers: r.allowed_headers.clone(),
                exposed_headers: r.exposed_headers.clone(),
                max_age: r.max_age,
            })
            .collect()
    }

    /// Run one request with the state locked. The error flag says whether the
    /// failure is answered as XML (service-level and ACL operations).
    fn dispatch(&self, req: &Request) -> Result<Response, (ApiError, bool)> {
        let target = routing::parse(&req.path);
        let xml = wants_xml(&target, req);
        // The table a table SAS must be signed for: the one in the URL or, for `/$batch`, `tn`.
        let table_name: Option<&str> = match &target {
            Target::Table(t)
            | Target::Query(t)
            | Target::Entity { table: t, .. }
            | Target::Nested { table: t, .. }
            | Target::BadKeys(t) => Some(t),
            Target::Batch => req.query.get("tn"),
            _ => None,
        };
        // The read-only secondary endpoint (`acct-secondary.table...`) refuses writes.
        let host = req.ctx.endpoint.authority.split('.').next().unwrap_or("");
        let secondary = !req.ctx.endpoint.path_style && host.to_ascii_lowercase().ends_with("-secondary");
        if secondary && !matches!(req.method, Method::GET | Method::HEAD) {
            return Err((ApiError::new(403, "InsufficientAccountPermissions", "Write operations are not allowed."), xml));
        }
        let mut st = self.state.lock();
        let acct = st.account(&req.account);

        let found = table_name.and_then(|n| acct.tables.get(&n.to_ascii_lowercase()));
        let grant = sas::authenticate(req, &self.accounts, table_name.map(|n| (n, found))).map_err(|e| (e, xml))?;
        let rq = Rq::new(req, grant);
        let result = route(&rq, acct, target).map_err(|e| (e, xml));
        // Only a read leaves the state alone. Inside the lock and after the change, so a save that sees the new
        // number also sees the change.
        if !matches!(req.method, Method::GET | Method::HEAD) {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
}

/// Operations whose errors are XML: everything with `comp=` / `restype=`.
fn wants_xml(target: &Target, req: &Request) -> bool {
    match target {
        Target::Root => true,
        Target::Table(_) => req.query.get("comp").is_some_and(|c| c.eq_ignore_ascii_case("acl")),
        Target::Nested { .. } => req.query.has("restype") || req.query.has("comp"),
        _ => false,
    }
}

fn route(rq: &Rq, acct: &mut state::Account, target: Target) -> Res<Response> {
    let req = rq.req;
    // `POST` with an `X-HTTP-Method` header stands in for PUT / MERGE / PATCH / DELETE on an entity.
    let tunnelled = match (&target, req.method.as_str(), req.header("x-http-method")) {
        (Target::Entity { .. }, "POST", Some(m)) => match m.trim().to_ascii_uppercase().as_str() {
            v @ ("PUT" | "MERGE" | "PATCH" | "DELETE") => Some(v.to_string()),
            _ => return Err(ApiError::new(400, "XMethodIncorrectValue", "The value specified in the X-HTTP-Method header is invalid.")),
        },
        _ => None,
    };
    let method = tunnelled.as_deref().unwrap_or(req.method.as_str());
    let comp = req.query.get("comp").map(str::to_ascii_lowercase);
    let area = match &target {
        Target::Root => "Service",
        Target::Tables => "Tables",
        Target::TableItem(_) => "TableItem",
        Target::Batch => "Batch",
        Target::Table(_) => "Table",
        Target::Query(_) => "Query",
        Target::Entity { .. } => "Entity",
        Target::Nested { .. } => "NestedPath",
        Target::BadKeys(_) => "BadKeys",
    };
    let unbuilt = || not_implemented(&format!("{} {}", req.method, req.decoded_path()), &format!("Table_Unknown ({method} {area})"));
    match target {
        Target::Root => {
            let service = req.query.get("restype").is_some_and(|r| r.eq_ignore_ascii_case("service"));
            match (method, service, comp.as_deref()) {
                ("GET", true, Some("properties")) => service_api::get_properties(rq, acct),
                ("PUT", true, Some("properties")) => service_api::set_properties(rq, acct),
                ("GET", true, Some("stats")) => service_api::stats(rq),
                _ => Err(unbuilt()),
            }
        }
        Target::Tables => match method {
            "GET" => table_api::query(rq, acct),
            "POST" => table_api::create(rq, acct),
            _ => Err(unbuilt()),
        },
        Target::TableItem(name) => match method {
            "DELETE" => table_api::delete(rq, acct, &name),
            "GET" => table_api::get(rq, acct, &name),
            _ => Err(unbuilt()),
        },
        Target::Batch => match method {
            "POST" => batch::handle(rq, acct),
            _ => Err(unbuilt()),
        },
        Target::Table(name) => {
            check_name(&name)?;
            match (method, comp.as_deref()) {
                ("POST", None) => entity_api::insert(rq, acct, &name),
                ("GET", Some("acl")) => service_api::get_acl(rq, acct, &name),
                ("PUT", Some("acl")) => service_api::set_acl(rq, acct, &name),
                _ => Err(unbuilt()),
            }
        }
        Target::Query(name) => {
            check_name(&name)?;
            match method {
                "GET" => entity_api::query(rq, acct, &name),
                _ => Err(unbuilt()),
            }
        }
        Target::Entity { table, pk, rk } => {
            check_name(&table)?;
            match method {
                "GET" => entity_api::get(rq, acct, &table, &pk, &rk),
                "PUT" => entity_api::update(rq, acct, &table, &pk, &rk, Mode::Replace),
                "PATCH" | "MERGE" => entity_api::update(rq, acct, &table, &pk, &rk, Mode::Merge),
                "DELETE" => entity_api::delete(rq, acct, &table, &pk, &rk),
                _ => Err(unbuilt()),
            }
        }
        Target::Nested { table, rest } => {
            check_name(&table)?;
            if req.query.has("restype") || req.query.has("comp") {
                return Err(ApiError::new(
                    400,
                    "InvalidQueryParameterValue",
                    "Value for one of the query parameters specified in the request URI is invalid.",
                ));
            }
            // a batch posted below a table path is refused before anything else
            if rest == "$batch" && req.method == Method::POST {
                return Err(ApiError::new(415, "MediaTypeNotSupported", "None of the provided media types are supported"));
            }
            api::table_ref(acct, &table)?;
            match routing::parse(&format!("/{rest}")) {
                // `/{table}/Tables`: an entity insert into a table that is not called "Tables"
                Target::Tables if req.method == Method::POST => Err(errors::needs_value()),
                Target::BadKeys(_) | Target::TableItem(_) => Err(bad_keys(rq, &table)),
                _ => Err(resource_not_found()),
            }
        }
        Target::BadKeys(table) => {
            check_name(&table)?;
            api::table_ref(acct, &table)?;
            Err(bad_keys(rq, &table))
        }
    }
}

/// A client that sends `DataServiceVersion` / `MaxDataServiceVersion` gets the
/// version of the answer back: 3.0 for a JSON body, 1.0 for an empty one.
fn with_data_service_version(mut resp: Response, req: &Request) -> Response {
    let negotiated = req.header("dataserviceversion").is_some() || req.header("maxdataserviceversion").is_some();
    let is_json = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/json"));
    let is_batch = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("multipart/"));
    if negotiated && resp.status().is_success() && (is_json || is_batch || resp.status() == 204) {
        let v = if is_json || is_batch { "3.0;" } else { "1.0;" };
        resp.headers_mut().insert("DataServiceVersion", azemu_core::http::HeaderValue::from_static(v));
    }
    resp
}

fn bad_keys(rq: &Rq, table: &str) -> ApiError {
    errors::invalid_input_msg(&format!(
        "The number of keys specified in the URI does not match number of key properties for the resource '{}.{}'.",
        rq.req.account, table
    ))
}

#[async_trait]
impl Service for TableService {
    fn name(&self) -> &'static str {
        "table"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("table")]
    }

    fn path_style_on_pinned(&self) -> bool {
        true
    }

    fn default_account(&self) -> &'static str {
        "devstoreaccount1"
    }

    async fn handle(&self, req: Request) -> Response {
        let preflight = req.method == Method::OPTIONS;
        let mut resp = if preflight {
            // CORS preflight: no credentials, judged against the service's CORS rules.
            azemu_core::storage::cors::preflight(&req, &self.cors_rules(&req.account))
        } else {
            match self.dispatch(&req) {
                Ok(r) => with_data_service_version(r, &req),
                Err((e, xml)) => render(&e, &req.ctx, xml),
            }
        };
        if !preflight && req.header("origin").is_some() {
            azemu_core::storage::cors::apply(&mut resp, &req, &self.cors_rules(&req.account));
        }
        finish(resp, &req, SERVER)
    }

    fn reset(&self) {
        let mut st = self.state.lock();
        *st = State::default();
        self.generation.fetch_add(1, Ordering::SeqCst);
        drop(st);
        self.accounts.reset();
    }

    /// Empty one storage account: its tables, entities and service properties. Persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        self.state.lock().accounts.retain(|k, _| !k.eq_ignore_ascii_case(account));
        self.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_state(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_state(dir)
    }
}
