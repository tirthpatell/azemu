//! The Schema Registry REST API, served on the namespace host under `/$schemaGroups` (on Azure
//! `https://{namespace}.servicebus.windows.net/$schemaGroups/...`). Everything here was compared with a Standard namespace in
//! November 2026; `docs/services/schema-registry.md` lists what and what could not be tried.
//!
//! | Request | Answer |
//! |---|---|
//! | `GET /$schemaGroups` | `{"NextLink":...,"Value":["group",...]}` |
//! | `GET /$schemaGroups/{group}` | the group's kind and mode (not in the REST specification, Azure answers it) |
//! | `GET /$schemaGroups/{group}/schemas` | the schema names of the group (the same) |
//! | `PUT /$schemaGroups/{group}/schemas/{name}` | 204 and the schema's headers; a new version when the content is new |
//! | `POST /$schemaGroups/{group}/schemas/{name}:get-id` | 204 and the headers of the version that has this content |
//! | `GET /$schemaGroups/{group}/schemas/{name}/versions` | `{"NextLink":...,"Value":[1,2]}` |
//! | `GET /$schemaGroups/{group}/schemas/{name}/versions/{n}` | the content and the headers (`0` is the latest version) |
//! | `GET /$schemaGroups/$schemas/{id}` | the same, by id |
//! | `DELETE .../schemas/{name}` and `.../versions/{n}` | 204, also for what is not there (not in the specification; Azure has it). On the schema it removes the latest version only |

use azemu_core::resp::Resp;
use azemu_core::time::{iso8601_secs, OffsetDateTime};
use azemu_core::{Request, Response};
use serde_json::{json, Value};

use crate::model::next_revision;
use crate::schema_registry::{parse_id, register_schema, Found, SchemaError, SchemaType};
use crate::state::Shared;

/// The `api-version`s the namespace host knows (Azure's own list, from its refusal of an unknown one); the registry answers all
/// of them the way it answers 2023-07-01.
pub const API_VERSIONS: &str = "2012-03,2012-08,2013-04,2013-07,2013-08,2013-10,2014-01,2014-05,2014-08,2014-09,2015-01,2015-04,2015-08,2016-03,2016-07,2017-04,2020-04,2020-09-01-preview,2021-10,2022-10,2023-07-01,2024-05";

/// Is this request for the registry? (The first segment of the path is `$schemaGroups`, in any case.)
pub fn is_registry_path(req: &Request) -> bool {
    req.segments().first().is_some_and(|s| s.eq_ignore_ascii_case("$schemaGroups"))
}

fn timestamp() -> String {
    iso8601_secs(OffsetDateTime::now_utc()).trim_end_matches('Z').to_string()
}

struct Ctx<'a> {
    ns: String,
    /// the path as it came, without the first slash
    path: String,
    api: String,
    base: String,
    req: &'a Request,
}

impl Ctx<'_> {
    fn text(&self, e: &SchemaError) -> String {
        let id = uuid::Uuid::new_v4();
        let n = id.as_bytes()[0] % 32;
        if e.gateway {
            format!(
                "{} TrackingId:{id}_G{n}, SystemTracker:{}.servicebus.windows.net:{}, Timestamp:{}",
                e.message,
                self.ns,
                self.path,
                timestamp()
            )
        } else {
            format!("{} TrackingId:{id}_D45, SystemTracker:NoSystemTracker, Timestamp:{}", e.message, timestamp())
        }
    }

    fn error(&self, e: SchemaError) -> Response {
        let message = self.text(&e);
        // a request whose content type is XML gets an XML answer to the refusal of that content type
        if e.code == "InvalidRequest" && e.message.starts_with("Missing required header") && self.is_xml() {
            let detail = message.replace('&', "&amp;").replace('<', "&lt;");
            return Resp::new(e.status)
                .header("content-type", "application/xml; charset=utf-8")
                .body(format!("<Error><Code>{}</Code><Detail>{detail}</Detail></Error>", e.status))
                .build();
        }
        Resp::new(e.status)
            .header("content-type", "application/json")
            .header("x-ms-error-code", e.code)
            .body(serde_json::to_vec(&json!({"error": {"code": e.code, "message": message}})).unwrap_or_default())
            .build()
    }

    fn is_xml(&self) -> bool {
        self.req.header("content-type").is_some_and(|c| c.to_ascii_lowercase().contains("xml"))
    }

    fn url(&self, path: &str) -> String {
        format!("{}/$schemagroups{path}?api-version={}", self.base, self.api)
    }

    fn found_headers(&self, mut r: Resp, f: &Found) -> Resp {
        let (g, s, v) = (&f.group, &f.schema, f.version.version);
        r = r
            .header("Location", self.url(&format!("/{g}/schemas/{s}/versions/{v}")))
            .header("Schema-Group-Name", g)
            .header("Schema-Name", s)
            .header("Schema-Id", &f.version.id)
            .header("Schema-Id-Location", self.url(&format!("/$schemas/{}", f.version.id)))
            .header("Schema-Version", v)
            .header("Schema-Versions-Location", self.url(&format!("/{g}/schemas/{s}/versions")));
        r
    }

    fn content_response(&self, f: &Found) -> Response {
        let ct = match f.version.kind {
            SchemaType::Avro => "application/json;serialization=Avro",
            SchemaType::Json => "application/json;serialization=Json",
            SchemaType::Protobuf => "text/vnd.ms.protobuf",
            _ => "text/plain; charset=utf-8",
        };
        let r = Resp::new(200u16).header("content-type", ct).body(f.version.content.clone());
        self.found_headers(r, f).build()
    }

    fn no_content(&self, f: &Found) -> Response {
        self.found_headers(Resp::new(204u16), f).build()
    }
}

fn list_response(body: Value) -> Response {
    Resp::new(200u16).header("content-type", "application/json").body(serde_json::to_vec(&body).unwrap_or_default()).build()
}

/// One page of a list the way Azure cuts it. `$skip` and `$top` are read; `NextLink` is `null` when `$skip` was given and nothing
/// follows (or no `$top` was), and otherwise a link whose `$skip` is the number of the last item taken (the number of items taken
/// less one, when the request had no `$skip`: that is how Azure's own links are made, checked in November 2026).
fn page<T: Clone>(ctx: &Ctx, items: &[T], path: &str) -> (Vec<T>, Value) {
    let skip: Option<usize> = ctx.req.query.get("$skip").and_then(|s| s.parse::<i64>().ok()).and_then(|n| usize::try_from(n).ok());
    let top_raw = ctx.req.query.get("$top");
    let top: Option<usize> = top_raw.and_then(|s| s.parse::<i64>().ok()).and_then(|n| usize::try_from(n).ok());
    let start = skip.unwrap_or(0).min(items.len());
    let rest = &items[start..];
    let taken = top.map(|t| t.min(rest.len())).unwrap_or(rest.len());
    let slice = rest[..taken].to_vec();
    let next_skip: Option<i64> = match skip {
        Some(s) => (top.is_some() && s + taken < items.len()).then_some((s + taken) as i64),
        None => Some(taken as i64 - 1),
    };
    let link = next_skip.map(|n| {
        let mut u = format!("{}/{path}?api-version={}", ctx.base, ctx.api);
        if let Some(t) = top_raw {
            u.push_str(&format!("&%24top={t}"));
        }
        u.push_str(&format!("&%24skip={n}"));
        Value::String(u)
    });
    (slice, link.unwrap_or(Value::Null))
}

pub fn handle(shared: &Shared, req: Request) -> Response {
    let raw: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
    let ctx = Ctx {
        ns: req.account.to_ascii_lowercase(),
        path: raw.join("/"),
        api: req.query.get("api-version").unwrap_or("").to_string(),
        base: req.ctx.endpoint.base_url(),
        req: &req,
    };
    let auth = shared.auth();
    let mut who = None;
    let early = if auth.is_strict() {
        match strict_login(shared, &auth, &ctx) {
            Ok(p) => {
                who = Some(p);
                None
            }
            Err(e) => Some(e),
        }
    } else {
        check_auth(&req)
    };
    if let Some(e) = early.or_else(|| check_api_version(&req)) {
        return ctx.error(e);
    }
    if let Some(p) = who {
        if let Some(e) = strict_role(shared, &auth, &ctx, &p) {
            return ctx.error(e);
        }
    }
    let answer = route(shared, &ctx);
    match answer {
        Ok(r) => r,
        Err(e) => ctx.error(e),
    }
}

fn check_auth(req: &Request) -> Option<SchemaError> {
    let unauthorized = |m: &str| SchemaError { status: 401, code: "ErrorInRequest", message: m.to_string(), gateway: true };
    match req.header("authorization") {
        None => Some(unauthorized("Missing authorization token.")),
        Some(h) if h.trim().len() > 7 && h.trim()[..7].eq_ignore_ascii_case("bearer ") => None,
        Some(_) => Some(unauthorized(
            "SubCode=40105, MalformedToken: Invalid authorization header: The request is missing WRAP authorization credentials.",
        )),
    }
}

// ---- strict authorisation (`docs/strict-auth.md`) ----------------------------------------------------------------------------

/// What was found on Azure in November 2026: a bearer token of Microsoft Entra ID is checked (`InvalidAudience`, `InvalidSignature`,
/// `MalformedToken`: all 401 `ErrorInRequest`); a shared access signature is not taken at all, whatever its key (403 `NotAvaliable`, the
/// spelling is Azure's, `SubCode=40301, Nullable object must have a value.`). An expired Entra token could not be made: its text is a guess.
fn strict_login(shared: &Shared, auth: &azemu_core::auth::Auth, ctx: &Ctx) -> Result<azemu_core::auth::Principal, SchemaError> {
    use azemu_core::auth::{Aud, TokenFailure};
    let unauthorized = |m: &str, gateway: bool| SchemaError { status: 401, code: "ErrorInRequest", message: m.to_string(), gateway };
    let Some(h) = ctx.req.header("authorization").map(str::trim).filter(|h| !h.is_empty()) else {
        return Err(unauthorized("Missing authorization token.", true));
    };
    let (scheme, rest) = h.split_once(' ').unwrap_or((h, ""));
    if scheme.eq_ignore_ascii_case("SharedAccessSignature") {
        return Err(SchemaError {
            status: 403,
            code: "NotAvaliable",
            message: "SubCode=40301, Nullable object must have a value.".into(),
            gateway: true,
        });
    }
    if !scheme.eq_ignore_ascii_case("bearer") || rest.trim().is_empty() {
        return Err(unauthorized(
            "SubCode=40105, MalformedToken: Invalid authorization header: The request is missing WRAP authorization credentials.",
            true,
        ));
    }
    let rns = shared.state.lock().resolve(&ctx.ns);
    let auds = [Aud::exact("https://eventhubs.azure.net"), Aud::exact("https://servicebus.azure.net"), Aud::host(&ctx.ns), Aud::host(&rns)];
    auth.authenticate_token(rest.trim(), ctx.req.ctx.time.unix_timestamp(), &auds).map_err(|f| match f {
        TokenFailure::Malformed => unauthorized("MalformedToken: .", true),
        TokenFailure::BadSignature => unauthorized("InvalidSignature: Invalid authorization token signature.", false),
        TokenFailure::WrongAudience(_) => unauthorized("InvalidAudience: Invalid authorization token audience.", false),
        other => unauthorized(azemu_core::auth::cbs::token_refusal(&other).text.split(" TrackingId:").next().unwrap_or(""), false),
    })
}

/// The roles. A call needs one of several actions (the text names them all): `SchemaGroupRead` is the `actions` entry
/// `Microsoft.EventHub/namespaces/schemagroups/read` (so the `Reader` role of Azure, whose actions are `*/read`, may list groups and
/// schemas), `SchemaRead` the data action `.../schemas/read`; `Schema Registry Reader` and `Contributor` carry both kinds. Listing the
/// groups, a group and its schema names needs `SchemaGroupRead` only; everything else, as found: reading a schema `SchemaGroupRead,SchemaRead`,
/// writing `SchemaGroupWrite,SchemaWrite`, deleting `SchemaGroupWrite,SchemaGroupDelete,SchemaDelete`.
fn strict_role(shared: &Shared, auth: &azemu_core::auth::Auth, ctx: &Ctx, who: &azemu_core::auth::Principal) -> Option<SchemaError> {
    use azemu_core::auth::{Need, Resource};
    let rns = shared.state.lock().resolve(&ctx.ns);
    let segs = ctx.req.segments();
    let rest: Vec<&str> = segs.iter().skip(1).map(String::as_str).collect();
    let method = ctx.req.method.as_str();
    const P: &str = "Microsoft.EventHub/namespaces";
    let (group, kind): (Option<&str>, &str) = match (method, rest.as_slice()) {
        ("GET", []) => (None, "group-read"),
        ("GET", ["$schemas", _]) => (None, "read"),
        ("GET", [g]) | ("GET", [g, "schemas"]) => (Some(g), "group-read"),
        ("DELETE", [g, ..]) => (Some(g), "delete"),
        ("PUT", [g, ..]) => (Some(g), "write"),
        ("POST", [g, ..]) => (Some(g), "read"),
        ("GET", [g, ..]) => (Some(g), "read"),
        _ => return None,
    };
    let (need, words) = match kind {
        "group-read" => (Need::control(&format!("{P}/schemagroups/read")), "SchemaGroupRead"),
        "read" => (
            Need::any(vec![Need::control(&format!("{P}/schemagroups/read")), Need::data(&format!("{P}/schemas/read"))]),
            "SchemaGroupRead,SchemaRead",
        ),
        "write" => (
            Need::any(vec![Need::control(&format!("{P}/schemagroups/write")), Need::data(&format!("{P}/schemas/write"))]),
            "SchemaGroupWrite,SchemaWrite",
        ),
        _ => (
            Need::any(vec![
                Need::control(&format!("{P}/schemagroups/write")),
                Need::control(&format!("{P}/schemagroups/delete")),
                Need::data(&format!("{P}/schemas/delete")),
            ]),
            "SchemaGroupWrite,SchemaGroupDelete,SchemaDelete",
        ),
    };
    let res = match group {
        Some(g) => Resource::event_hubs(&rns).at(format!("schemagroups/{g}")),
        None => Resource::event_hubs(&rns),
    };
    auth.authorize(&format!("{method} /{}", ctx.path), who, &need, &res).err().map(|_| SchemaError {
        status: 401,
        code: "ErrorInRequest",
        message: format!("Authorization failed for specified action: {words}."),
        gateway: true,
    })
}

fn check_api_version(req: &Request) -> Option<SchemaError> {
    if req.query.raw().is_empty() {
        return Some(SchemaError::invalid("No query parameter found. At least api-version should be provided."));
    }
    match req.query.get("api-version") {
        None | Some("") => Some(SchemaError::invalid("api-version should be provided.")),
        Some(v) if !API_VERSIONS.split(',').any(|k| k == v) => Some(SchemaError::invalid(format!(
            "SubCode=40000, The api-version in the query string is not supported. Either remove it from the Uri or use one of '{API_VERSIONS}'."
        ))),
        Some(_) => None,
    }
}

/// The checks Azure makes of a name in the path.
fn check_name(ctx: &Ctx, name: &str) -> Result<(), SchemaError> {
    if !ctx.path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '$' | ':' | '/')) {
        return Err(SchemaError::invalid(format!(
            "Uri path '/{}' contain invalid characters. Schema name and schema group name must consisted of letters, numbers, '-', and '.' only.",
            ctx.path
        )));
    }
    if name.contains('$') {
        return Err(SchemaError::invalid(format!("Invalid character '$' detected in name '{name}'.")));
    }
    if name.chars().count() > 100 {
        return Err(SchemaError::invalid("Name cannot exceed 100 characters."));
    }
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    if !(edge(name.chars().next()) && edge(name.chars().last())) {
        return Err(SchemaError::invalid("Name must start and end with letter or digit."));
    }
    Ok(())
}

/// A name segment cut at its `:` (`name:get-id`).
fn split_action(seg: &str) -> (&str, Option<&str>) {
    match seg.split_once(':') {
        Some((n, a)) => (n, Some(a)),
        None => (seg, None),
    }
}

fn version_number(v: &str) -> Result<i64, SchemaError> {
    v.parse::<i64>().map_err(|_| SchemaError::invalid(format!("Schema version {v} is not a valid version number.")))
}

/// The serialization named by the content type of a `PUT` or `POST`.
fn schema_kind(ctx: &Ctx) -> Result<SchemaType, SchemaError> {
    let method = ctx.req.method.as_str();
    let Some(ct) = ctx.req.header("content-type") else { return Err(SchemaError::invalid("Content-Type is required.")) };
    let lower = ct.to_ascii_lowercase();
    let mut parts = lower.split(';').map(str::trim);
    let media = parts.next().unwrap_or("");
    let serialization = parts.find_map(|p| p.strip_prefix("serialization=").map(|v| v.trim().trim_matches('"').to_string()));
    match (media, serialization.as_deref()) {
        ("text/vnd.ms.protobuf", _) => Ok(SchemaType::Protobuf),
        ("text/plain", _) => Ok(SchemaType::Custom),
        ("application/json", Some("avro")) => Ok(SchemaType::Avro),
        ("application/json", Some("json")) => Ok(SchemaType::Json),
        ("application/json", Some(other)) => Err(SchemaError {
            status: 415,
            code: "InvalidSchemaType",
            message: format!("Invalid schema type for {method} request. '{other}' is not supported."),
            gateway: true,
        }),
        _ => {
            Err(SchemaError::invalid(format!("Missing required header for {method} request. Schema/serialization type must be provided.")))
        }
    }
}

fn body_text(ctx: &Ctx) -> Result<String, SchemaError> {
    if ctx.req.body.is_empty() {
        return Err(SchemaError::invalid("Error when consuming request body: The body of the message cannot be read because it is empty."));
    }
    Ok(String::from_utf8_lossy(&ctx.req.body).into_owned())
}

fn not_a_path(ctx: &Ctx) -> Response {
    let m = ctx.req.method.as_str();
    if m == "PUT" || m == "POST" {
        let e = SchemaError::invalid(format!(
            "The given path {} is not a valid path for SchemaRegistry protocol handler's Put or Post request.",
            ctx.path
        ));
        return ctx.error(e);
    }
    if m == "HEAD" {
        return Resp::new(400u16).header("x-ms-error-code", "InvalidRequest").build();
    }
    azemu_core::json_error::response(
        501,
        "NotImplemented",
        &format!("eventhubs: the Schema Registry request {m} /{} is not built", ctx.path),
    )
}

fn route(shared: &Shared, ctx: &Ctx) -> Result<Response, SchemaError> {
    let segs = ctx.req.segments();
    let rest: Vec<&str> = segs.iter().skip(1).map(String::as_str).collect();
    let method = ctx.req.method.as_str();
    let ns = &ctx.req.account;
    // a registration locks the state itself, for the short steps only (`register_schema`)
    if let ("PUT", [group, "schemas", seg]) = (method, rest.as_slice()) {
        check_name(ctx, group)?;
        let (name, _) = split_action(seg);
        check_name(ctx, name)?;
        let kind = schema_kind(ctx)?;
        let text = body_text(ctx)?;
        let f = register_schema(shared, ns, group, name, kind, &text, ctx.req.body.len())?;
        return Ok(ctx.no_content(&f));
    }
    let mut st = shared.state.lock();
    match (method, rest.as_slice()) {
        ("GET", []) => {
            let names: Vec<String> =
                st.namespaces.get(&st.resolve(ns)).map(|n| n.schema_groups.values().map(|g| g.name.clone()).collect()).unwrap_or_default();
            let (slice, link) = page(ctx, &names, "$schemaGroups");
            Ok(list_response(json!({"NextLink": link, "Value": slice})))
        }
        ("GET", ["$schemas", id]) => {
            let Some(found) = parse_id(id).map(|i| (i.chars().all(|c| c == '0'), i)) else {
                return Err(SchemaError::invalid("The request is invalid. Invalid schema Id format."));
            };
            if found.0 {
                return Err(SchemaError {
                    status: 400,
                    code: "InvalidRequest",
                    message: "SubCode=40000, Missing schema id as input.".into(),
                    gateway: false,
                });
            }
            match st.schema_by_id(ns, &found.1) {
                Some(f) => Ok(ctx.content_response(&f)),
                None => Err(SchemaError::not_found(format!("Schema id {id} does not exist."))),
            }
        }
        (_, [group, ..]) => {
            check_name(ctx, group)?;
            route_group(&mut st, ctx, group, &rest[1..])
        }
        _ => Ok(not_a_path(ctx)),
    }
}

fn route_group(st: &mut crate::state::State, ctx: &Ctx, group: &str, rest: &[&str]) -> Result<Response, SchemaError> {
    let method = ctx.req.method.as_str();
    let ns = &ctx.req.account;
    let gpath = group;
    match (method, rest) {
        ("GET", []) => match st.schema_group(ns, group) {
            Some(g) => {
                let t = |ms: i64| {
                    let t = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap_or(OffsetDateTime::UNIX_EPOCH);
                    format!(
                        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
                        t.year(),
                        t.month() as u8,
                        t.day(),
                        t.hour(),
                        t.minute(),
                        t.second(),
                        t.nanosecond() / 1000
                    )
                };
                Ok(list_response(json!({
                    "UpdatedAtUtc": t(g.updated_ms),
                    "CreatedAtUtc": t(g.created_ms),
                    "ETag": g.etag,
                    "GroupProperties": g.properties,
                    "SchemaCompatibility": g.compatibility.name(),
                    "SchemaType": g.schema_type.name(),
                })))
            }
            None => Err(SchemaError::not_found(format!("Schema {group} does not exist."))),
        },
        ("GET", ["schemas"]) => {
            let names: Vec<String> =
                st.schema_group(ns, group).map(|g| g.schemas.values().map(|s| s.name.clone()).collect()).unwrap_or_default();
            let (slice, link) = page(ctx, &names, &format!("$schemaGroups/{gpath}/schemas"));
            Ok(list_response(json!({"NextLink": link, "Value": slice})))
        }
        (m, ["schemas", seg, tail @ ..]) => {
            let (name, action) = split_action(seg);
            check_name(ctx, name)?;
            match (m, action, tail) {
                ("POST", Some("get-id"), []) | ("POST", None, [":get-id"]) => {
                    let kind = schema_kind(ctx)?;
                    let text = body_text(ctx)?;
                    let f = st.schema_id_for_content(ns, group, name, kind, &text)?;
                    Ok(ctx.no_content(&f))
                }
                ("DELETE", _, []) => {
                    st.delete_latest_schema_version(ns, group, name);
                    next_revision();
                    Ok(Resp::new(204u16).build())
                }
                ("GET", _, ["versions"]) => {
                    let versions: Vec<u32> = st
                        .schema_group(ns, group)
                        .and_then(|g| g.schemas.get(&crate::model::key(name)))
                        .map(|s| s.versions.iter().map(|v| v.version).collect())
                        .unwrap_or_default();
                    let (slice, link) = page(ctx, &versions, &format!("$schemaGroups/{gpath}/schemas/{name}/versions"));
                    Ok(list_response(json!({"NextLink": link, "Value": slice})))
                }
                ("GET", _, ["versions", v]) => {
                    let n = version_number(v)?;
                    match st.schema_version(ns, group, name, n) {
                        Some(f) => Ok(ctx.content_response(&f)),
                        None => Err(SchemaError::not_found(format!("Schema {group}/{name}/ver.{v} does not exist."))),
                    }
                }
                ("DELETE", _, ["versions", v]) => {
                    let n = version_number(v)?;
                    if let Ok(n) = u32::try_from(n) {
                        st.delete_schema_version(ns, group, name, n);
                    }
                    next_revision();
                    Ok(Resp::new(204u16).build())
                }
                _ => Ok(not_a_path(ctx)),
            }
        }
        _ => Ok(not_a_path(ctx)),
    }
}
