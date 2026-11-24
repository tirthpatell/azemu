# Strict authorisation (`--strict-auth`)

A missing role assignment is one of the most common failures when code first meets real Azure: the infrastructure code forgot `Storage Blob Data Contributor`,
or gave `Reader` where a data role was needed. By default the emulator takes any bearer token, so no test can catch that. Strict mode catches it, and answers
with what Azure answers.

```bash
azemu --strict-auth                    # or AZEMU_STRICT_AUTH=1
azemu --strict-auth --auth-config roles.json
```

It is **off by default**, and with it off nothing changes: a bearer token is never looked at, as before. With it on, the start-up log says so and lists the
services that do and do not check roles; `GET /_azemu/config` shows `"strictAuth": true` and `GET /_azemu/auth` lists the services again.

**It is not a security boundary.** The admin API can give any role to anyone, and anything that can reach the port can call it. The mode shows you the mistakes
a program makes; it does not keep anybody out ([`SECURITY.md`](../SECURITY.md)).

## What it checks, and what it does not

What a request with a bearer token goes through:

1. **The token.** It must be one the emulator's identity service issued: the signature is checked with the identity service's key, `exp` and `nbf` are
   honoured, and `aud` must be an audience the service takes on Azure (for Storage `https://storage.azure.com` or the account's own URL; for Key Vault
   `https://vault.azure.net` or the vault's own URL; and so on: [Audiences](#audiences)). A token that is missing, malformed, badly signed, expired or made for another
   resource is refused with the answer of that service.
2. **The role.** The caller is the token's `oid`. The operation needs a data action; the registry says whether any role assignment of that principal, at a scope
   that covers the resource, grants it. Services that have their own system (Cosmos DB roles, Key Vault access policies) use that.

What stays as it was, in strict mode too:

- **The emulator's own calls between its services** are not checked: Resource Manager making a container or a queue for a resource it was asked to make, and the seed file
  (`--seed`). They are marked inside the process, for the one call, and nothing a client sends can ask for the mark: a request from the network that carries the text the
  emulator once used there (`Bearer azemu-arm`, `Bearer azemu-seed`) is an ordinary bearer token, and not a valid one, so it is refused as any other bad token is. (The Event Grid
  dead-letter write and the calls Resource Manager makes into Service Bus and Event Hubs do not go through a request at all.)
- A request with **no** credential keeps today's answer (401 with the challenge).
- A request with an **account key, a SAS, a connection string or a Cosmos DB / App Configuration key** keeps today's checks (the README, "Credentials are checked
  loosely"). On Azure a key is full access too: strict mode is about identities and roles. **Service Bus and Event Hubs are the exception**: their shared access keys
  have rights (`Listen`, `Send`, `Manage`) and an expiry, so strict mode checks them too ([Service Bus, Event Hubs and Kafka](#service-bus-event-hubs-and-kafka)).
- **Resource Manager itself is not checked.** Any token may call the management plane; it stores and serves role assignments whatever the mode is (a Terraform
  run must not depend on it).
- The **tenant** of a token is not compared (there is one made-up tenant). A token from another tenant could not be tried on Azure either.
- **Conditions (ABAC)** on a role assignment are stored and ignored.
- **Deny assignments** do not exist (the emulator has none; Azure has them for blueprints and managed applications).
- **Data Lake ACLs.** Azure checks the POSIX ACLs of a path when no role grants the access; strict mode checks roles only.
- **Network rules, private endpoints, `allowSharedKeyAccess = false`, `disableLocalAuth`** and similar account settings are not looked at.
- A role assignment is in force at once (Azure takes a few minutes at times).

Services:

| Service | In strict mode |
|---|---|
| Blob, Data Lake (`dfs`), Queue, Table | checked: [Storage](#storage) |
| Key Vault (role-based and access-policy vaults) | checked: [Key Vault](#key-vault) |
| App Configuration | checked: [App Configuration](#app-configuration) |
| Cosmos DB | checked, with its own roles: [Cosmos DB](#cosmos-db) |
| Event Grid, publishing to a custom topic | checked: [Event Grid](#event-grid) |
| Resource Manager, Identity | not checked (see above) |
| Service Bus (AMQP and the administration API), Event Hubs (AMQP), Kafka, the Schema Registry | checked, with shared access keys and tokens: [Service Bus, Event Hubs and Kafka](#service-bus-event-hubs-and-kafka) |
| Monitor, File (Application Insights, Azure Monitor, Azure Files) | **not checked**; the start-up log names them |

A service that does not check roles takes any bearer token as before. The start-up log always lists them (`strict authorisation is on, but these services do not
check roles ...`), so a green test is never read as "checked" by mistake.

## Audiences

The `aud` of a token must be one of these, for the service the request is for (case and a trailing slash do not matter). A URL is taken only as a whole: the name of the
account, vault, store, topic or namespace, then one of the domains below. A token for `https://acct1.vault.azure.net` is not good for the storage account `acct1`, and neither is one
for `https://acct1.evil.example`: both are a wrong audience. Where the first column says "tried", Azure took it in November 2026 (`tests/sdk-python/auth/test_auth_tokens.py`); the rest is
the documentation, and the other clouds' domains.

| Service | The service's own resource | The URL of the resource (`{name}` is the account, vault, store, topic or namespace the request names) |
|---|---|---|
| Blob | `https://storage.azure.com` (tried) | `https://{name}.blob.core.windows.net` (tried; also `.core.chinacloudapi.cn`, `.core.usgovcloudapi.net`, `.core.cloudapi.de`) and the emulator's `{name}.blob.core.localhost`, `{name}.blob.localhost`, `{name}.blob.core.windows.net.localhost` |
| Queue, Table | `https://storage.azure.com` | the same with `queue` or `table` for `blob` (Table: also `{name}.table.cosmos.azure.com`) |
| Data Lake | `https://storage.azure.com` | the same with `dfs` or `blob` for `blob` |
| Key Vault | `https://vault.azure.net`, and the resource the emulator's own challenge named (`https://{vault}.vault.localhost` form) | `https://{name}.vault.azure.net` (also `.vault.azure.cn`, `.vault.usgovcloudapi.net`, `.vault.microsoftazure.de`, and `managedhsm.` in the same clouds), `{name}.vault.localhost`, `{name}.vault.azure.net.localhost` |
| App Configuration | `https://azconfig.io`, `https://appconfig.azure.com`, and the domain of the endpoint the SDK was given (`https://azconfig.localhost`) | `https://{name}.azconfig.io` (also `.azconfig.azure.cn`, `.azconfig.azure.us`), `{name}.azconfig.localhost`, `{name}.azconfig.io.localhost` |
| Cosmos DB | `https://cosmos.azure.com` | `https://{name}.documents.azure.com`, `{name}.sql.cosmos.azure.com`, `{name}.sqlx.cosmos.azure.com` (the three Azure named in its answer to a wrong audience; also `.documents.azure.cn`, `.documents.azure.us`) and `{name}.documents.localhost` |
| Event Grid (topics) | `https://eventgrid.azure.net` | `https://{name}.{region}-1.eventgrid.azure.net` (the region is a label of its own; also `.azure.cn`, `.azure.us`), `{name}.eventgrid.localhost` |
| Service Bus, Event Hubs, Kafka, the Schema Registry | `https://servicebus.azure.net` and `https://eventhubs.azure.net` (either for both: the emulator takes both) | `https://{name}.servicebus.windows.net` (also `.servicebus.chinacloudapi.cn`, `.servicebus.usgovcloudapi.net`, `.servicebus.cloudapi.de`) and the emulator's `{name}.servicebus.localhost`, `{name}.eventhub.localhost`, `{name}.eventhubs.localhost`. A Kafka client names the bootstrap host (`https://{name}.servicebus.windows.net`) |

A name under `--tls-name` or `--allow-host` is **not** an audience of its own (`https://acct1.blob.corp.example`): ask for the service's resource (`https://storage.azure.com/.default`).

## Giving a role

The roles are held in one registry. There are four ways in; what comes in by any of them is the same object.

### Terraform (or any Resource Manager request)

`azurerm_role_assignment` works. It looks the role up by name (`GET .../providers/Microsoft.Authorization/roleDefinitions?$filter=roleName eq '...'`), creates the
assignment under a GUID (`PUT {scope}/providers/Microsoft.Authorization/roleAssignments/{guid}`) and reads it back; the Cosmos DB form is
`azurerm_cosmosdb_sql_role_assignment`. Role definitions and assignments are served at the scope of a subscription, a resource group, a resource and a part of a
resource, in Resource Manager's shapes, whether the mode is on or not (`docs/services/arm.md`). `tests/terraform/cases/authz` is a working configuration.

A storage account, vault, store, Cosmos DB account, namespace or topic made through Resource Manager has a resource id, and an assignment at that id, at its
resource group or at its subscription applies to it. Narrower scopes apply to parts of it as Azure documents:

| Scope below the resource | Applies to |
|---|---|
| `.../blobServices/default/containers/{c}` | that container (and a Data Lake file system, which is the same thing) |
| `.../queueServices/default/queues/{q}` | that queue |
| `.../tableServices/default/tables/{t}` | that table |
| `.../vaults/{v}/secrets/{name}`, `/keys/{name}`, `/certificates/{name}` | that one object |

### The admin API

A request with a body (`PUT`, `POST`) is JSON: send `Content-Type: application/json`. From a browser any other type is a `415`, here as for every admin path ([`../SECURITY.md`](../SECURITY.md)).

For an account that was **not** made through Resource Manager (the emulator makes accounts on first use, so such an account has no resource group) and for tests that
have no Terraform:

```bash
curl -X PUT localhost:4577/_azemu/auth/assignments -H 'Content-Type: application/json' -d '{
  "clientId": "<client id>", "role": "Storage Blob Data Reader", "scope": "storage/myaccount/containers/data" }'
```

| Request | |
|---|---|
| `PUT /_azemu/auth/assignments` | give a role; 201 (200 when it was already there) with the assignment in Resource Manager's shape |
| `GET /_azemu/auth/assignments[?principal=&clientId=&role=&scope=]` | list |
| `DELETE /_azemu/auth/assignments?name=<guid>` (or `principal=`, `clientId=`, `role=`, `scope=` to filter, `all=true` for every one) | take roles away |
| `GET /_azemu/auth/roles[?name=]`, `PUT /_azemu/auth/roles`, `DELETE /_azemu/auth/roles?id=` | role definitions; a custom role by `PUT` with `name`, `dataActions`, `notDataActions`, `actions`, `notActions` |
| `GET /_azemu/auth/check?principal=&action=&scope=[&kind=control]` | what the registry decides, and which assignments it looked at |
| `GET/PUT/DELETE /_azemu/auth/cosmos/assignments` | Cosmos DB's own role assignments (`account`, a principal, `role` = `Reader`, `Contributor` or an id, `scope` = `/`, `/dbs/{db}` or `/dbs/{db}/colls/{c}`) |
| `GET/PUT/DELETE /_azemu/auth/keyvault/policies` | access policies of a vault Resource Manager did not make (`vault`, `policies`: a principal and the permission words per kind) |

**Who.** One of `principalId` (an object id; `principal` is the same), `clientId` (resolved to its object id as below) or `managedIdentity: true`.
`principalType`, `name` (the GUID of the assignment; made from the principal, the role and the scope when left out, so giving the same role twice is no error),
`condition` and `description` are optional.

**Which role.** A built-in or custom role by name or by id.

**Where.** Either a full Azure id, or a short form that names the service, the account and optionally the part:

| Short form | Resource |
|---|---|
| `storage/{account}`, `storage/{account}/containers/{c}`, `/queues/{q}`, `/tables/{t}` | a storage account, or one container (or file system), queue or table |
| `keyvault/{vault}`, `keyvault/{vault}/secrets/{n}`, `/keys/{n}`, `/certificates/{n}` | a vault, or one object in it |
| `appconfig/{store}` | an App Configuration store |
| `eventgrid/{topic}` | a custom topic |
| `servicebus/{namespace}`, `/queues/{q}`, `/topics/{t}` | a Service Bus namespace, queue or topic |
| `eventhubs/{namespace}`, `/eventhubs/{hub}` | an Event Hubs namespace or one event hub |

An account that Resource Manager did **not** make has no resource group, so it is covered only by an assignment that names it (a short form, or a full id that ends
in the account). An assignment at a resource group or a subscription does not reach it.

### A file

`--auth-config <file>` / `AZEMU_AUTH_CONFIG` holds assignments that exist from the start and after every reset, like `--seed`. The JSON is the one of the admin API:

```json
{
  "assignments": [
    {"clientId": "<client id>", "role": "Storage Blob Data Reader", "scope": "storage/myaccount/containers/data"},
    {"managedIdentity": true, "role": "Key Vault Secrets User", "scope": "keyvault/myvault"}
  ],
  "roles": [{"name": "Blob lister", "dataActions": ["Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read"]}],
  "cosmosAssignments": [{"account": "myaccount", "clientId": "<client id>", "role": "Contributor", "scope": "/dbs/app"}],
  "vaults": [{"vault": "myvault", "policies": [{"clientId": "<client id>", "secrets": ["get", "list"]}]}]
}
```

A mistake in the file stops the start and the message names the file, the entry and the reason.

### What is kept

With `--data-dir` the assignments and custom roles made through Resource Manager or the admin API are kept in `<data-dir>/auth/` (`auth.state`, kind `auth`,
version 1). What comes from the file is put back from the file; what Resource Manager keeps itself (its resources, the id of each data resource, the access
policies of a vault, Cosmos DB's role definitions and assignments) comes back with Resource Manager's state. A data directory of version 0.2.0 has no `auth`
directory and starts with no assignments.

## Who the principal is

The identity service gives every client id a stable object id (`oid`), and a token carries it. To give a role to a client id without decoding a token:

```bash
curl 'localhost:4577/_azemu/identity/principal?client_id=<client id>'
# {"clientId":"<client id>","objectId":"<oid>","tenantId":"<tenant>","named":"clientId"}
curl localhost:4577/_azemu/identity/principal                    # the default managed identity (no query)
curl 'localhost:4577/_azemu/identity/principal?object_id=<oid>'  # a managed identity asked for by its object id
```

The managed identity is the one `IDENTITY_ENDPOINT` hands tokens to when it names no client id. The admin API takes `"managedIdentity": true`, or `"clientId"`
with the value above, so it never needs the object id by hand.

## Built-in roles

The roles that carry rights for the emulated services are there with their real ids and the actions Microsoft publishes
([Azure built-in roles](https://learn.microsoft.com/azure/role-based-access-control/built-in-roles)): `Owner`, `Contributor`, `Reader` (they carry **no** data
actions: that a `Contributor` cannot read a blob is exactly the mistake the mode catches), the Storage Blob (`Owner`, `Contributor`, `Reader`, `Delegator`), Queue
(`Contributor`, `Reader`, `Message Processor`, `Message Sender`, `Delegator`) and Table (`Contributor`, `Reader`, `Delegator`) roles, Key Vault `Administrator`,
`Reader`, `Secrets Officer`, `Secrets User`, `Crypto Officer`, `Crypto User`, `Crypto Service Encryption User`, `Crypto Service Release User`, `Certificates Officer`,
`Certificate User` and `Data Access Administrator`, `App Configuration Data Owner` and `Data Reader`, `Azure Service Bus Data Owner`, `Sender` and `Receiver`,
`Azure Event Hubs Data Owner`, `Sender` and `Receiver`, `Schema Registry Contributor` and `Reader`, and `EventGrid Data Sender`. `GET /_azemu/auth/roles` lists them with
their ids and actions. Cosmos DB has its own two (`00000000-0000-0000-0000-000000000001` Reader and `...0002` Contributor). A role the emulator does not know is
an error in a request (`RoleDefinitionDoesNotExist`); an *action* it does not know, in a custom role or a check, is not an error.

How the rules are applied, as Azure documents them: an assignment applies to its scope and everything below it; what a principal may do is the union over all its
assignments that cover the resource; a `*` in an action matches any run of characters (a `/` too); `notDataActions` take away from the data actions of their own role
definition only. The storage calls that Microsoft lists with an `actions` entry and not a data action (the container, queue and table calls themselves: list, create, delete, metadata,
ACL; the service properties; Get User Delegation Key; Get Account Information) look at `actions` and `notActions`; every call about the data in them (a blob, a message, an entity) looks at `dataActions` only. That is why a Data Reader may list containers and ask for a
user delegation key (Azure did the same) while a `Contributor`, whose `actions` are `*`, may too, and still cannot read a blob.

## Finding out why a request was refused

A refusal is logged at the default level, one line:

```
INFO refused GET /data/b1: principal 6f1c... has no role assignment that grants Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read on /subscriptions/.../storageAccounts/acct/blobServices/default/containers/data (the principal has 2 role assignment(s), but none applies to ...; GET /_azemu/auth/check explains)
```

and `GET /_azemu/auth/check?principal=<oid>&action=<data action>&scope=<id or short form>` answers what the registry would decide, with every assignment of the
principal it looked at (its scope, its role, whether it applies, what it grants) and a sentence for the reason. Add `kind=control` for an `actions` entry, or
`cosmosAccount=<name>` to ask about a Cosmos DB role. With the first line of the log and one call, the missing assignment is found.

## What was compared with Azure, per service

Everything below was tried against real Azure in November 2026 with four service principals (every data role, the read roles, no role, and roles on single things), each
with its own token, unless it says otherwise. `tests/sdk-python/auth` states one row of what Azure answered per test, and the **same tests run against both**: the
emulator (113 tests) and Azure (102 tests; the 11 that check what only the emulator has, or what was not probed on Azure, are skipped there). The messaging part is `test_servicebus.py` (20 tests), `test_eventhubs.py` (9), `test_kafka.py` (9) and `test_schemaregistry.py` (8, one of them emulator-only), which create nothing on Azure's management plane and leave the queues empty (a send to an event hub stays in the hub until its retention ends: the runs left six events of 4 bytes). Where Microsoft's tables and what Azure did disagree, what Azure
did wins, and the difference is named here.

### Storage

Blob, Queue, Table and Data Lake. The operation-to-action table is Microsoft's "Permissions for calling data operations"
([Authorize with Microsoft Entra ID](https://learn.microsoft.com/rest/api/storageservices/authorize-with-azure-active-directory)).

- **A missing role** is 403 `AuthorizationPermissionMismatch` (create container and create queue: `AuthorizationFailure`, with a shorter text), and the message
  never names an action. Blob and Queue answer in XML, Table in OData JSON **without** `x-ms-error-code`, Data Lake in JSON with it. HEAD has no body.
- **The order:** the role is checked before the body is read and before the blob or container is looked up: a principal with no role gets 403 for a blob that does not
  exist, a reader gets 404. Exceptions, as on Azure: Set Service Properties reads its body first (an invalid XML body is 400 even for a principal with no role), and
  Delete Table looks for the table first (404 for a table that is not there, for every principal).
- **A Data Reader may do more than read:** list with every `include` but `tags` (that needs `blobs/tags/read` too), get a user delegation key, list containers and
  read the service properties (the last through the `Reader` role of the same principal). A Data Reader of queues may peek and list, not receive (Get Messages
  needs `messages/process/action` or `delete` and `read`).
- **A blob batch** is accepted (202) and each part is judged on its own: a part the principal may not do is `403` in the multipart answer.
- **A role on one container** applies inside it and nowhere else; outside it, a blob that exists and one that does not are both 403. The account-level calls (list
  containers, user delegation key, service properties) are 403.
- **A token that is wrong:** the answer depends on `x-ms-version`. From 2019-12-12 (Blob, Queue; 2020-12-06 for Table; 2017-11-09 for Data Lake) it is 401
  `InvalidAuthenticationInfo` with `WWW-Authenticate: Bearer authorization_uri=... resource_id=https://storage.azure.com` and an `AuthenticationErrorDetail` (`Audience
  validation failed. Audience did not match.`, `Signature validation failed. Signature verification failed.`, `The access token was missing or malformed.`); before it,
  403 `AuthenticationFailed` and no challenge. A version before 2017-11-09 (or none) does not take a bearer token at all: 403 with no `x-ms-error-code`. Table has no detail
  text. The Python Table SDK sends 2019-02-02 and so sees the 403 form.
- **Copy and rename: the source is judged too.** Microsoft's table names `blobs/read` on the source of Copy Blob, Put Blob From URL, Put Block From URL, Append Block From URL and
  Put Page From URL, next to the write on the destination, and `move/action` on the source of a Data Lake rename. The emulator asks for them: a principal with a role on the
  destination container only cannot copy `secret/b.txt` into it, however the source is written (the account in another case, with `-secondary`, with an escaped name, in the path
  style: the copy reads all of these as the same account, and so does the check). Whose right it is: a source with a SAS in its URL is read with what the SAS says (as for any request, a
  SAS is read for its form and its terms); `x-ms-copy-source-authorization: Bearer <token>` is the principal of *that* token, which must be good for Storage and have the read on
  the source (a token that is not good is 401, one without the right 403); a source in the account of the request is read by the principal of the request; a source in another
  account with neither is up to the account's public access, as without strict mode. A rename needs `move/action` and `read` on the file system it comes from (a source with a SAS
  is read with the SAS). The answer for a source the caller may not read is `CannotVerifyCopySource` (403, or 401 for a token that does not verify, with `CopySourceStatusCode`,
  `CopySourceErrorCode` and `CopySourceErrorMessage` in its details) for a copy, and `AuthorizationPermissionMismatch` for a rename. **What Azure answers for an unauthorised
  source was not probed**; these are the documented codes (`tests/sdk-python/auth/test_auth_copy_source.py` is emulator-only for that reason).
- **A user delegation SAS** (made with a user delegation key) is checked like any SAS: for its form and its terms. The roles of the principal whose key signed it (`skoid`) are not
  looked at, so a SAS can grant what that principal could not.
- **Not the same as Microsoft's table:** Put Blob and Snapshot need `write` *or* `add/action` (the emulator does not look whether the blob exists first, so a
  principal with `add/action` only may also overwrite); Delete Blob of a version needs `deleteBlobVersion/action` on Azure and `delete` here; Table batches are judged
  per operation and the first refused operation fails the whole change set (what Azure sends for a refused part of a batch was not tried); the Data Lake operations take
  the Blob actions (`blobs/read`, `write`, `delete`, `move/action`, `modifyPermissions/action`).

### Key Vault

Role-based vaults and vaults with access policies (the vault says which: `enableRbacAuthorization` of a vault made through Resource Manager; any other vault is
role-based; `PUT /_azemu/auth/keyvault/policies` gives policies to such a vault).

- **Role-based:** 403 `Forbidden` with the action and the resource in the text and `innererror.code` `ForbiddenByRbac`. List calls name the vault, calls about one object
  name the object. `delete` and `keys/read` have no `/action`. The check is the first thing after the token and before the object is looked up (a missing secret is 403
  for a principal that may not read it, 404 for one that may).
- **A Secrets User** may read deleted secrets (with the value) and list them (Azure did, and `readMetadata`/`getSecret` of the role say so).
- **Access policies:** 403 with the permission word (`secrets get`, `keys list`, ...) and `ForbiddenByPolicy` for a principal that has an entry and lacks the
  permission, `AccessDenied` for one with no entry. Updating a secret needs `set`.
- **A role on one secret** applies to that secret and to the list of its versions.
- **A token that is wrong:** 401 with `WWW-Authenticate: Bearer authorization=..., resource=...` and `AKV10022: Invalid audience. ...`, `[Unknown] Error validating token:
  'S2S12086'.` (signature) or `[MalformedToken] Error validating token: 'S2S12086'.`.

### App Configuration

403 and 401 with **no body** (and no `Content-Type`): the 401 carries `WWW-Authenticate: HMAC-SHA256, Bearer error="invalid_token", error_description="Authorization
token failed validation"`. A Data Reader may read everything (keys, labels, revisions, snapshots) and nothing else; a missing key is 403 for a principal with no role and 404
for a reader. The audiences are `https://azconfig.io`, `https://appconfig.azure.com`, the store's own URL and the domain of the endpoint the SDK was given.

### Cosmos DB

Its own role system (`sqlRoleDefinitions`, `sqlRoleAssignments`, scopes at the account, a database or a container), with three kinds of 403: `5300` for a call that is not
available with a token at all (making or deleting a database or container, users, offers, listing or making scripts; the same for every principal, a contributor
too), `5301` for a metadata read without `readMetadata` (a client cannot even be built), `5302` for an item, query, change feed or stored procedure call without the
action (the text names the action and Azure's operation and resource type numbers). A token that is wrong is 401 with `5003`, `5004` or `5008`. The names of a database and a
container in a role's scope are compared with their case (as Cosmos DB names are): `/dbs/App` is not `/dbs/app`. The account in the scope is not.

### Event Grid

Publishing to a custom topic with a token needs `EventGrid Data Sender` on the topic. **Every refusal is 401** (not 403), with no `WWW-Authenticate`. Event Grid
namespaces take any credential as before.

### Service Bus, Event Hubs and Kafka

These take two kinds of credential, and strict mode checks both (all of it only with the mode on; with it off every credential is accepted, as before).

**Shared access keys.** A rule (a shared access policy) has a name, rights (`Listen`, `Send`, `Manage`) and two keys, on a namespace or on one queue, topic or event hub.
Where the keys come from:

- A namespace **made through Resource Manager** has the rules Resource Manager holds (`authorizationRules`, the root rule `RootManageSharedAccessKey` included), with the keys
  `listKeys` hands out: one function makes both, and `regenerateKeys` takes effect at once.
- Any other namespace (the emulator makes them on first use) has the rule `RootManageSharedAccessKey` with the fixed development key **`SAS_KEY_VALUE`**, the value Microsoft's own
  Service Bus and Event Hubs emulators show in their connection string
  (`Endpoint=sb://localhost;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=SAS_KEY_VALUE;UseDevelopmentEmulator=true`). The key is the text itself, not base64.
- More rules are declared in the service's config file or through its admin API, in the way the entities are:

```bash
curl -X PUT localhost:4577/_azemu/servicebus/myns/authorization-rules/reports \
  -H 'Content-Type: application/json' -d '{"rights": ["Send"], "primaryKey": "my-key"}'                     # a namespace rule
curl -X PUT localhost:4577/_azemu/eventhubs/myns/authorization-rules/ingest \
  -H 'Content-Type: application/json' -d '{"rights": ["Send", "Listen"], "entity": "hub1"}'                 # a rule of one event hub; the keys are made from where it is when left out
```

  `GET` lists them with their keys, `DELETE .../{name}[?entity=]` removes one. In the config file (`AZEMU_SERVICEBUS_CONFIG`, `AZEMU_EVENTHUBS_CONFIG`) a namespace, a queue,
  a topic or an event hub takes `"AuthorizationRules": [{"Name": "reports", "Rights": ["Send"], "PrimaryKey": "my-key", "SecondaryKey": "..."}]`; they come back after a reset.
  A rule declared through the admin API is kept in memory only (the config file is what survives a restart).

What is checked, with Azure's answers (November 2026):

- **A SAS token** (`SharedAccessSignature sr=...&sig=...&se=...&skn=...`, at `$cbs` `put-token`, in the `Authorization` header of the Service Bus administration API, or as a
  connection string key in Kafka's SASL PLAIN password): the key name must exist on the namespace or on the entity the token is used for, the signature must be made with one of
  its two keys, `sr` must name this namespace (the first label of its host: `sb://otherns.servicebus.windows.net/q` is `InvalidAudience`, which matters because every namespace Resource Manager did not make has the same
  development key) and cover the entity (a namespace covers all, an entity covers itself and what is below it: `http` and `sb` are the same), and `se` must not be more than
  **five minutes** past (Azure took 290 s late and refused 300 s). Each way to fail has its text (`InvalidSignature` for an unknown name, a wrong key and a rule of another
  entity; `InvalidAudience`; `ExpiredToken`), at `put-token` with status 401 and `com.microsoft:auth-failed`.
- **A connection holds at most 1,000 credentials** at once (one per audience; a renewal replaces its own): the 1,001st `put-token` is refused (`403`, `amqp:resource-limit-exceeded`), and a `name` longer than
  1,024 bytes is too. These are the emulator's own limits; Azure's were not probed.
- **The rights** are checked at the **link attach**, not at `put-token`: a token that is good but lacks the claim is accepted, the attach is answered and the link is detached
  with `amqp:unauthorized-access` and `Unauthorized access. 'Send' claim(s) are required to perform this operation. Resource: 'sb://<namespace>.servicebus.windows.net/<entity>'`
  (`Listen` for a receiver, a peek and the partition of a consumer group). `Manage` also counts as `Send` and `Listen` (a guess: Azure always gives them together).
- **A token runs out while a link is open:** the links that depended on it are detached with `amqp:unauthorized-access` unless a newer token for that audience came. The condition
  is a guess (an Entra token cannot be made to expire on Azure, and the wait for a SAS token to run out was not tried).
- **Entra tokens** (a JWT at `$cbs`, as `OAUTHBEARER`, or `Bearer` on the administration API): the signature, expiry and audience (`https://servicebus.azure.net`,
  `https://eventhubs.azure.net` or the namespace's own URL), then roles at the attach: `Azure Service Bus Data Sender` / `Receiver` / `Owner` and `Azure Event Hubs Data Sender` /
  `Receiver` / `Owner`, at the namespace, at one queue, topic or event hub (at the resource group or the subscription for a namespace Resource Manager made). A principal with no
  role passes `put-token` and is refused at the attach; on Event Hubs its `$management` read is answered with a 401 inside the answer (`amqp:unauthorized-access`, "Attempted to perform an
  unauthorized operation."), and a rule or role that may send or receive may read.
- **The Service Bus administration API** answers 401 where 403 would be expected. A rule needs `Manage` for everything about an entity (a rule of one entity is no rule for another
  entity or for a list); the namespace information is open to every rule. A token needs the *`actions`* of a role, not its data actions (`.../queues/read`, which is why a Data Receiver, a
  Data Sender on that one queue and a `Reader` may get a queue, and nobody with a data role may make one).
- **Kafka**: SASL PLAIN (user `$ConnectionString`, the connection string as the password) and SASL OAUTHBEARER; a failed login is answered and the connection is closed; a produce needs `Send`
  and a fetch, a list of offsets and an offset fetch or commit need `Listen`: `TOPIC_AUTHORIZATION_FAILED` (29) per partition. A principal with no role at all has the connection closed at
  the coordinator lookup (a consumer) or after the produce answer. A request before the login is not answered.
- **The Schema Registry**: a bearer token needs `Schema Registry Reader` / `Contributor` (or any role with the same `actions`: a `Reader` may list groups and read); a SAS token is not taken at all.
  Every refusal is 401 `ErrorInRequest` and names the actions (`Authorization failed for specified action: SchemaGroupWrite,SchemaWrite.`).

What stays loose: the **tenant** of a token, `disableLocalAuth`, a namespace's network rules, and the namespace of a Kafka connection string against the host the client dialled (only compared
when the client used TLS with a server name). The Event Hubs HTTP send API is not built, so it has nothing to check.

### Could not be confirmed

- **A copy or a rename whose source the caller may not read**, and a source token that is not good: the codes are the documented ones, what Azure sends was not probed (see
  [Storage](#storage)). The audiences of Queue, Table and Data Lake were not tried either: only the Blob endpoint's own URL was.
- **Expired and not-yet-valid tokens.** An Entra token cannot be made to expire (its life is about an hour). The emulator honours `exp` and `nbf`, and the answer texts of
  each service for them are guesses from the family of the others: Storage `Lifetime validation failed. The token is expired.`, Key Vault, Cosmos DB (`5005`) and the
  rest are not from a recorded answer. (`tests/sdk-python/auth/test_auth_emulator.py` tests the Storage one against the emulator only.)
- **A token from another tenant.** No second tenant was available.
- **The read-only keys** of Cosmos DB and App Configuration. The emulator does not tell the two keys of Cosmos DB apart, so writing with the read-only key is not refused
  (Azure answers 401); the App Configuration read-only key could not be tried.
- **Resource Manager's own answers** for a role assignment (the management plane was not probed in this work): `RoleAssignmentExists` (409), `RoleDefinitionDoesNotExist`
  (400), `ResourceGroupNotFound` / `ResourceNotFound` (404) for a scope that does not exist, and `InvalidRequestContent` (400) for a principal id that is not a GUID are the
  documented codes and the usual shape of Resource Manager errors; the texts are written from the documentation and from memory. `RoleAssignmentUpdateNotPermitted`,
  `RoleDefinitionHasAssignments`, `InvalidRoleAssignmentName` and `BuiltInRoleReadOnly` are guesses of the same kind.
- **Cosmos DB role assignments narrower than the account**, and a scoped principal with a Cosmos DB role (the scoped principal had none).
- **Table batches, Event Grid with a correctly formed `aeg-sas-token`, other API versions, other tiers, other regions.**
- **Service Bus, Event Hubs, Kafka (checked in November 2026 with real credentials, except for what follows).** Not tried on Azure: expired and not-yet-valid Entra tokens (the texts are
  the family of the SAS ones); a link that is open when its token runs out (the condition `amqp:unauthorized-access` and its text are guesses); a `Manage` claim on a link; the claim a
  `$management` request of Service Bus needs per operation (Azure's SDKs attach a sender or a receiver first, and that is where the refusal came); what a rule with `Manage` only (no `Send`)
  may do; topics, subscriptions and sessions with their own rules or roles; the order of the checks when a token is expired *and* for another entity (the emulator checks the key, the
  signature, the audience, then the expiry); an administration call that deletes an *existing* entity without the right (the text `Manage,EntityDelete` is a guess); Kafka admin calls
  (`CreateTopics`, `DeleteTopics`, topics made by a producer's metadata request need `Manage` here) and `DescribeConfigs`; a SAS token for the Schema Registry with a correct key (every one
  was refused the same way); Event Hubs `Manage`; the audience `https://eventhubs.azure.net` for Service Bus and the other way round (the emulator takes both); a Kafka connection that Azure
  closes after a produce by a principal with no role (the emulator closes it after the answer).

## For a service that is not HTTP

A service that takes its calls some other way (AMQP, Kafka) asks the same registry; Service Bus, Event Hubs and Kafka do. `azemu_core::auth` has what it needs
(`sas` for the rules and the check of a token, `cbs` for what `put-token` and an attach need, `claims` for what a connection holds):

```rust
let auth: azemu_core::auth::Auth = ...;                      // the handle full_stack() gives every service that checks roles
if auth.is_strict() { ... }
let who = auth.authenticate_token(&token, now_unix, &[Aud::exact("https://servicebus.azure.net"), Aud::host(&namespace)])?; // Result<Principal, TokenFailure>
let res = Resource::service_bus(&namespace).at(format!("queues/{queue}"));
auth.authorize("attach sender", &who, &Need::data("Microsoft.ServiceBus/namespaces/messages/send/action"), &res)?; // logs a refusal in one line
auth.registry().may(&who.oid, &need, &res);                    // the same without the log; check(...) gives the decision with the assignments it looked at
```
