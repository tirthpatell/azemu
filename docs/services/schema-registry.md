# Schema Registry (part of `azemu-eventhubs`)

**What it emulates.** The Schema Registry of an Event Hubs namespace: the REST API that the Azure serializers (`azure-schemaregistry`, `azure-schemaregistry-avroencoder`,
`Azure.Data.SchemaRegistry`, the Java and JavaScript clients, the Kafka serializers that call the same REST API) use to register and fetch schemas. It is served on the namespace's
own host, under `/$schemaGroups`: `https://{namespace}.eventhub.localhost:{port}/$schemaGroups/...` (on Azure `https://{namespace}.servicebus.windows.net/$schemaGroups/...`). Groups,
schemas and versions are part of the namespace's state.

**How to reach it.** Point the client at `{namespace}.eventhub.localhost:{port}` (the gateway port, 4577 by default) and give it a credential that returns any token. The SDKs send
a bearer token only over TLS, so trust the emulator's certificate authority (`~/.azemu/ca.pem`, or `--cert-dir`). In Python:

```python
from azure.schemaregistry import SchemaRegistryClient
client = SchemaRegistryClient(
    fully_qualified_namespace="myns.eventhub.localhost:4577",
    credential=AnyTokenCredential(),            # any object with get_token() that returns an AccessToken
    connection_verify="~/.azemu/ca.pem"  # the path, expanded,
)
```

**Groups are not made through this API.** On Azure a schema group is made on the management side (Resource Manager, the portal), and so it is here:

| Way | How |
|---|---|
| Admin API | `PUT /_azemu/eventhubs/{ns}/schemagroups/{group}` (`Content-Type: application/json`) with `{"schemaType":"Avro","schemaCompatibility":"Forward","groupProperties":{"k":"v"}}` (all optional: Avro, None). 201 when made, 200 when changed (the kind cannot change once the group holds schemas). `GET` and `DELETE` on the same path, `GET .../schemagroups` lists. |
| Config file | `AZEMU_EVENTHUBS_CONFIG`: `"SchemaGroups": [{"Name": "orders", "SchemaType": "Avro", "SchemaCompatibility": "Forward"}]` in a namespace, next to `Entities`. Made again after a reset. |
| Resource Manager | `Microsoft.EventHub/namespaces/schemagroups` (`properties.schemaType`, `schemaCompatibility`, `groupProperties`; tried with raw requests, not with Terraform). Deleting the resource deletes the group and its schemas. |
| `--lenient` | `AZEMU_EVENTHUBS_AUTOCREATE` (or `AZEMU_LENIENT=1`): a group named by a registration comes into being (type from the content type, compatibility None), as event hubs do. A convenience Azure does not have. Without it a missing group is Azure's 404. |

`schemaType` is `Avro`, `Json`, `Custom`, `Protobuf` or `Unknown` (a group that takes every kind); `schemaCompatibility` is `None`, `Backward` or `Forward`.

## Operations

| Request | Answer |
|---|---|
| `GET /$schemaGroups` | `{"NextLink":...,"Value":["group",...]}` |
| `PUT /$schemaGroups/{group}/schemas/{name}` | 204 and the headers below. The content is the body, the kind is the `Content-Type`. The same content (white space and key order do not count) gives the same id and version; changed content is a new version. |
| `POST /$schemaGroups/{group}/schemas/{name}:get-id` (also `.../{name}/:get-id`) | 204 and the headers of the version that has this content; 404 when there is none |
| `GET /$schemaGroups/{group}/schemas/{name}/versions` | `{"NextLink":...,"Value":[1,2]}` |
| `GET /$schemaGroups/{group}/schemas/{name}/versions/{n}` | the schema and the headers; `0` is the latest version |
| `GET /$schemaGroups/$schemas/{id}` | the same, by id (32 hex digits or a GUID with dashes, any case) |
| `GET /$schemaGroups/{group}`, `GET /$schemaGroups/{group}/schemas` | the group's kind and mode, the schema names (not in the REST specification; Azure answers both) |
| `DELETE .../schemas/{name}`, `DELETE .../versions/{n}` | 204, also for what is not there (not in the specification either; Azure has it). On the schema it removes the **latest version only**; the schema is gone with its last version. |

Content types: `application/json; serialization=Avro`, `application/json; serialization=Json`, `text/plain; charset=utf-8` (custom), `text/vnd.ms.protobuf`. A schema is answered
with `application/json;serialization=Avro` (no space), `...Json`, and the last two as they are. The answer headers of a registration, a `get-id` and every read: `Schema-Id` (32 lower-case
hex digits, new for every version), `Schema-Version`, `Schema-Group-Name`, `Schema-Name`, `Location` (`.../$schemagroups/{group}/schemas/{name}/versions/{n}?api-version=...`),
`Schema-Id-Location`, `Schema-Versions-Location`; the URLs use the host the client called. `api-version`: `2021-10`, `2022-10`, `2023-07-01` (the newest in the REST specification, with
Protobuf) and `2024-05` are answered alike; so is every other version Azure's own refusal lists (Azure answered `2017-04` with older `X-Schema-*` headers; the emulator does not).
The URLs in the headers carry the version the client sent.

Lists take `$skip` and `$top`. `NextLink` is copied from what Azure sends, odd as it is: without `$skip` the link is always there and its `$skip` is the number of the last item taken
(`$skip=0` after one item, `-1` after none); with `$skip` it is `null` unless `$top` left items out.

## What is checked, in Azure's order

1. Credentials: no `Authorization` header is 401 `ErrorInRequest` "Missing authorization token."; a header that is not `Bearer ...` is 401 "MalformedToken". **Any bearer token passes**,
   as everywhere in the emulator.
2. `api-version`: none at all (no query string) "No query parameter found. At least api-version should be provided.", empty or missing "api-version should be provided.", unknown
   "SubCode=40000, The api-version in the query string is not supported. Either remove it from the Uri or use one of '2012-03,...'." (400 `InvalidRequest`).
3. Names (group, schema): letters, digits, `-`, `_` and `.`; a `$` is "Invalid character '$' detected in name '...'"; more than 100 characters (the REST specification says 50; Azure
   takes 100); not starting and ending with a letter or digit; anything else (a space as `%20`) "Uri path '...' contain invalid characters". A `:` ends the name (`name:get-id`).
4. For `PUT` and `POST`: the `Content-Type` ("Content-Type is required."; `application/json` without `serialization=` or another type "Missing required header for PUT request. Schema/serialization type must be provided.",
   answered in XML when the request was XML; an unknown serialization 415 `InvalidSchemaType` "Invalid schema type for PUT request. 'thrift' is not supported."), then the body
   ("Error when consuming request body: The body of the message cannot be read because it is empty.").
5. The content: Avro must be a JSON object that Avro's .NET parser takes: "Avro schema validation failed: ..." with its text (`Property type is required at ''.`, `name cannot be null for record schema. at ''.`,
   `'fields' cannot be null for record at ''.`, `'type' was not found for field: name at 'fields[0]'.`, `Enum has no symbols: E at ''.`, `Array does not have 'items' at ''.`, a duplicate field, a `fixed` without `size`).
   What that parser lets through is let through here: a record called `1bad`, a default that does not fit its type, `{"type":"foo"}` (a type that names nothing). A Json schema must be JSON. The stored
   and returned form is the compact JSON in the order it came; two contents are one schema when they are the same JSON.
6. The group: missing is 404 `ItemNotFound` "SubCode=40400, Schema group '...' does not exist while auto-create is turned off."; a kind the group does not take is 415 `InvalidSchemaType` "SubCode=41500, The messaging entity
   'Schema of type Json cannot be created within group '...' that supports Avro schema type.' could not be found. ..." (the text is Azure's, odd as it reads).
7. Size: more than 1,048,576 bytes is 403 `NotAvaliable` (sic) "SubCode=40300, Cannot create schema ... because size of schema data cannot exceed 1048576 bytes. Right now ... data is ... bytes."
8. Compatibility, for an Avro schema in a group in Forward or Backward mode that already has a version of that name: against the **latest version only** (not against all of them), and the latest content again is not
   checked. Refused: 400 `InvalidRequest` "Avro schema validation failed: Forward compatibility test failed when checking against schema {name}, version {n}, id {guid}." The content of an *older* version that
   does not fit the latest one is refused too; one that fits is answered with that older version's id. The check runs before the lookup of existing content.

Every error is `{"error":{"code":...,"message":"... TrackingId:..., SystemTracker:{namespace}.servicebus.windows.net:{path}, Timestamp:..."}}` with `x-ms-error-code`; an error of the entity layer says `NoSystemTracker`.
Unknown names read as 404 `ItemNotFound` ("Schema {group}/{name}/ver.{n} does not exist.", "Schema id ... does not exist."); the version list of an unknown schema or group is an empty list, not an error.
Version `abc` is 400 "Schema version abc is not a valid version number."; an id that is not an id is 400 "The request is invalid. Invalid schema Id format."

### The compatibility check is not Azure's

Azure's code is not available, so the check is Avro's own rule (can the reader read what the writer wrote?; Forward: reader = the latest version, writer = the new content; Backward: the other way) with
the differences found by registering about sixty pairs of schemas in a Forward group on a real namespace, each pair taken from a list of changes (`schema_avro.rs`, `forward_pairs_as_azure_answered`):

* The type of a field of the top record must be the same: `int` to `long`, `float` to `double`, `string` to `bytes`, a union against a plain type and a union with a branch of another type are all refused
  there, although Avro allows the widening. Further down (a nested record, an array, a map, a schema that is a number itself) the widening is applied.
* An enum is compared by its name only: symbols added, removed or all replaced are accepted. A renamed record, enum or fixed, a renamed field (also with `aliases`), a changed namespace and a changed `size` are refused; `aliases` are ignored.
* A removed field needs a `default` in the old schema, an added field needs nothing (Forward). A `logicalType` must be the same.

What was **not** tried: Backward mode (the namespace's group is Forward; the check is the same function with the roles swapped and its message says "Backward"), Avro features such as `aliases` on types, recursive
records and `fixed` with logical types beyond the pairs above. A change Azure accepts that the emulator refuses (or the other way round) is a bug of this table; the pairs are cheap to add to the test.

### Limits of the emulator's own check

The check is stopped, and the registration answers `400 InvalidRequest` with "Avro schema validation failed: the compatibility check was stopped ...", when the schemas **nest more than 64
levels** (a name that leads to a record counts like the record written in place) or the comparison needs **more than 200,000 steps**. Both are the emulator's own limits: Azure's code is not
available and nothing like them was found by trying. A real schema needs a few hundred steps. The check remembers the pairs of records it has found compatible, so a schema in which many
fields have the same type is compared at once. The JSON reader of the registry stops at 128 levels of brackets: a deeper schema is `400` ("The schema is nested more than 128 levels deep").
The check runs with the namespace unlocked: other requests of the namespace do not wait for it. A schema may have at most **1,000 versions** (the emulator's own limit; `403 NotAvaliable`, "SubCode=40300 ... no more
than 1000 versions of a schema are allowed"); finding the version that has some content does not read the stored versions again.

## Limits (Standard tier, Microsoft Learn "Event Hubs quotas and limits")

| Limit | In the emulator |
|---|---|
| 1 MB per schema | **enforced** (the per-request limit), with Azure's text |
| 100 characters in a schema name, 50 in a group name | enforced for schema names; group names are checked when the group is made |
| One schema group (excluding the default group), 25 schema versions across all groups, 25 MB for the registry | **not enforced** (the emulator's own cap is 1,000 versions of one schema, above). Azure answers the 26th with 403 `NotAvaliable` "SubCode=40300, Cannot create schema {name} in group {group} because no more than 25 schemas is allowed. Currently there are 25 schemas for the group." |
| 1,024 properties per group, 256 bytes per key, 1,024 per value | not enforced |

A real Standard namespace is full quickly: tests that run against it must delete what they register (the SDKs have no delete; a `DELETE` as above does it).

## State, reset, persistence

The registry belongs to the namespace. `POST /_azemu/reset` and `POST /_azemu/reset?service=eventhubs&account={namespace}` drop its groups and schemas (the config file's groups come back); deleting the namespace through
Resource Manager drops it too. With `--data-dir` it is kept in `eventhubs/schemas.state` (kind `eventhubs-schemas`, version 1; no file exists while no namespace has a group; a directory from before the registry loads as it did).
Ids, version numbers and the order of versions survive a restart. [`persistence.md`](persistence.md).

## Kafka

Nothing was built in the Kafka endpoint for it. **Azure's registry is not the Confluent Schema Registry:** its REST API is the one above, not Confluent's (`/subjects`, `/schemas/ids/{id}`, `/config`). A Kafka
client that uses Confluent's serializers cannot talk to it, on Azure or here; Microsoft's Kafka serializers (`azure-schema-registry-for-kafka`) call the REST API above, so they should work with the namespace host as the registry URL (not tried).
The Kafka endpoint itself is in [`kafka.md`](kafka.md).

## What was tried

* **Azure** (a Standard namespace, one group in Avro/Forward, November 2026, raw requests and the Python SDK): every operation above with its answers and headers, well over a hundred requests for the errors (unknown group, schema, id and version; bad Avro of eleven
  kinds; wrong, missing and unknown content types; names; sizes; api-versions, also 2021-10, 2022-10 and 2024-05; no token, a malformed one; the roles Schema Registry Reader and no role), paging, `DELETE`, and about 60 pairs for the compatibility rule.
  `tests/sdk-python/schemaregistry` runs against it with `AZEMU_AZURE=1` (21 tests, the two that check what only the emulator has are skipped).
* **SDKs against the emulator:** Python `azure-schemaregistry` 1.3.0 (sync and async, api versions 2021-10 and 2022-10) and `azure-schemaregistry-avroencoder` 1.0.0 (encode, send through Event Hubs, receive, decode, two versions on one stream). There is no
  Azure SDK client for the registry in Go or Rust (checked in November 2026: the Go repository has none; the Rust crate `azure_data_schemaregistry` is an empty 0.0.1), so those suites have nothing for it. The .NET, Java and JavaScript clients were not tried.
* **Rust tests** of every operation and error: `crates/services/eventhubs/tests/schema_registry.rs`, the file format in `tests/state_v1.rs` (`tests/state-v1/eventhubs-schemas/`), Resource Manager in `crates/services/arm/tests/schema_groups.rs`.

## Strict authorisation

With `--strict-auth` a bearer token is checked (`InvalidAudience`, `InvalidSignature`, `MalformedToken`: 401 `ErrorInRequest`) and the principal needs a role. The roles are the ones
of Azure: `Schema Registry Reader` and `Contributor` carry both an `actions` entry (`.../schemagroups/read`, `.../schemagroups/*`) and a data action (`.../schemas/read`, `.../schemas/*`), and a
call is let through by any of them (so a plain `Reader` may list groups, read a group and its schema names, and read a schema; checked on Azure). A refusal is 401 `ErrorInRequest` with the
actions that would have done: `SchemaGroupRead` (list the groups, a group, the schema names), `SchemaGroupRead,SchemaRead` (a schema, its versions, a read by id, `get-id`),
`SchemaGroupWrite,SchemaWrite` (register), `SchemaGroupWrite,SchemaGroupDelete,SchemaDelete` (delete). A role on one group (`.../schemagroups/{group}`) covers that group only. A shared access
signature is not taken at all, whatever its key: 403 `NotAvaliable` (Azure's spelling) "SubCode=40301, Nullable object must have a value.". All checked against Azure in November 2026.

## Known differences

* **Roles are not checked unless `--strict-auth` is on** (then they are, with the answers below; `docs/strict-auth.md`). Azure answers a principal with only the reader role that tries to register or delete with 401 `ErrorInRequest` "Authorization failed for specified action: SchemaGroupWrite,SchemaWrite."
  (`SchemaGroupWrite,SchemaGroupDelete,SchemaDelete` for a delete), and one without any role "Authorization failed for specified action: SchemaGroupRead." (a list) or "SchemaGroupRead,SchemaRead." (a read). The emulator lets every token do everything.
* The compatibility check differs from Azure's in cases not on the list above (see "The compatibility check is not Azure's").
* The wording of a JSON syntax error comes from a different parser (Azure's is Newtonsoft's, "Unexpected end of content while loading JObject. Path 'type', line 1, position 16."); the emulator word-for-word copies only `[` and the
  cases of the list above, and gives a similar sentence for the rest.
* The limits per namespace are not enforced (above). API versions before 2020 are answered as 2023-07-01.
* `Schema-Id` is a random identifier here. On Azure it was not the MD5, SHA-1 or SHA-256 of the text or of Avro's canonical form that was tried; the same content under another name has another id there too.

## Behaviour that could not be confirmed against Azure

The namespace used has one Avro group, so these are guesses:

* Groups of type `Json`, `Custom`, `Protobuf` and `Unknown`: what they take, and the content types of their answers (the emulator answers `application/json;serialization=Json`, `text/plain; charset=utf-8`, `text/vnd.ms.protobuf`). Whether a Json
  schema is compared as JSON (the emulator does: white space does not make a new version) or as text, and whether a Json schema is compared for compatibility (the emulator does not; Learn says compatibility is for Avro only).
* The Backward mode (above), and what a group in `Unknown` type does.
* `POST ...:get-id` with an unknown serialization or an XML content type says "POST request" where Azure's text for `PUT` says "PUT request".
* The page size of a list that has many items (the emulator never cuts a list that has no `$top`).
