# App Configuration

**What it emulates.** The data-plane REST API of Azure App Configuration (JSON, `api-version` query parameter): key-values with labels and
tags, locks, revisions, point-in-time reads, snapshots, keys and labels lists. Feature flags and Key Vault references are stored as plain
key-values, as the service does.

**How to reach it.** `http(s)://{store}.azconfig.localhost:4577` (the store is the first label of the host; any name is a store of its own and is
made on first use). With `--standard-ports`: `https://azemu:10011`, where the store is the host name (`localhost` or an address: the store
`default`). A connection string with HMAC works over plain `http`; a token credential needs `https` and the emulator's certificate authority.
How to reach the emulator from a container, and how to trust the CA: `docs/reaching-the-emulator.md`.

**Not built.** `PATCH /kv/{key}`, CORS preflight and any other method or path answer 501 and name the operation. No throttling (429 for request
rates) and no key or value size limit beyond the 1 MiB request limit. Tokens and HMAC signatures are never checked.

**Key-values from a file.** A seed file (`--seed`, [`docs/seed.md`](../seed.md)) sets the key-values you list (`key`, optional `label`, `value`, optional `contentType`) at start and
after every reset. A key-value that already exists (from `--data-dir`, say) is not set again, so it gets no new revision.

Crate `azemu-appconfig`, service name `appconfig`, mount `AccountHost("azconfig")`. Wire format (status codes, header names, JSON field order,
error bodies) was copied from the 304 recordings in `refs/recordings/python/sdk/appconfiguration/` and the 2024-09-01 / 2026-04-01 specs.

## Pointing the Azure SDK at it

Connection string (HMAC, plain http is fine; the secret must be valid base64):

```python
from azure.appconfiguration import AzureAppConfigurationClient
client = AzureAppConfigurationClient.from_connection_string(
    "Endpoint=http://mystore.azconfig.localhost:4577;Id=any;Secret=YXplbXU=")
```

Entra ID (bearer token; the SDK only sends tokens over https, so use the https endpoint and
trust the emulator CA, `~/.azemu/ca.pem` or `GET /_azemu/ca.pem`):

```python
client = AzureAppConfigurationClient(
    "https://mystore.azconfig.localhost:4577", credential=AnyTokenCredential(),
    connection_verify="/home/me/.azemu/ca.pem")
```

Tokens and HMAC signatures are never checked. A name under `.localhost` resolves on a Linux desktop only: from a container, or on Windows
and on macOS before 26, use `--standard-ports` (`docs/reaching-the-emulator.md`).

## Operations

| Operation | State |
|---|---|
| `GET /kv`, `HEAD /kv` (key, label, tags filters, `$select`, `After`, `Accept-Datetime`, `snapshot`, page `If-None-Match`) | built |
| `GET/HEAD/PUT/DELETE /kv/{key}` (label, `$select`, `Accept-Datetime`, `If-Match`, `If-None-Match`) | built |
| `PUT/DELETE /locks/{key}` | built |
| `GET/HEAD /keys`, `GET/HEAD /labels` | built |
| `GET/HEAD /revisions` (filters, `$select`, `Accept-Datetime`, paging, `Range: items=a-b`) | built |
| `PUT /snapshots/{name}`, `GET/HEAD /snapshots/{name}`, `PATCH /snapshots/{name}`, `GET/HEAD /snapshots` | built |
| `GET /operations?snapshot={name}` | built (always `Succeeded`) |
| `OPTIONS` (CORS preflight), `PATCH /kv/{key}`, any other method or path | 501 problem+json naming the operation |

Feature flags and Key Vault references are plain key-values; content type and value are stored
and returned untouched.

## Behaviour worth knowing

- Create and overwrite of a setting both answer 200. `DELETE` answers 200 with the deleted setting, or 204 when missing.
- 404 and 412 have an empty body (as recorded). 409 on a locked setting is `application/problem+json` with the recorded text.
- A null label and a named label are different settings. On a single-key call, an absent label, `label=` and `label=%00` mean the null label.
- Lists are ordered by key (byte order), the null label first. A page has at most 100 items. A full page always has a
  `Link: <...>; rel="next"` header and `@nextLink` in the body, even when the next page turns out empty (as recorded: no look-ahead).
  The cursor `after` is base64 of `key\nlabel`.
- The page `ETag` is a hash of the item etags; `If-None-Match` of it (in quotes) answers 304 (with the `Link` header, no `ETag`). Only a value in
  quotes (or `*`) is a condition on a list: a bare etag is passed over, as Azure does. `If-Match` on a list: `*`, the page's etag or the etag of a
  setting on the page is 200, another quoted value is `412`.
- Filter grammar (`key`, `label`, `name`): exact, `prefix*`, `*suffix`, `*contains*`, `*`, comma lists, `\` escapes, the null label as
  `\0` / `%00`. A `*` in the middle is a 400 `invalid-argument` with `pos`. `tags=name=value` (repeatable, all must match).
- `/revisions` is newest first across all keys. Every write, lock and unlock is a revision; deleted settings keep theirs.
- Newer api-versions (after 2024-09-01) add `description` to items and snapshots and the `x-ms-item-count` header on lists.
- `api-version` is checked against what Azure took in November 2026: `1.0`, `2022-11-01-preview`, `2023-10-01`, `2023-11-01`, `2024-09-01`, `2026-04-01`.
  Another value that looks like a version (`2022-11-01`, `2025-01-01`, `2.0`) is `400` "Unsupported API version"; text that is no version is "Invalid
  API version" (the `detail` names the request URI and the version). **No `api-version` at all is taken**, and so are `1.0` and `2022-11-01-preview`, in
  the older shape: a list has no `etag` in its body.
- Auth: any `Authorization: HMAC-SHA256 ...` or `Bearer ...` is accepted. No header (or another scheme) gives `401` with no body and
  `WWW-Authenticate: HMAC-SHA256, Bearer`, as Azure does. A wrong HMAC signature or a bearer token that is not a token is `401` on Azure and passes here:
  the emulator has no key for a store (stores come into being by being named) and no tenant to ask.
- Errors of a request that cannot be read are `400 invalid-argument` with the parameter named and an empty `detail`: `key` for a body that is not JSON,
  `tags` for tags of the wrong type or a `tags=` filter that is not `name=value`, `filters` for a snapshot without usable filters or with more than 3.
  Six comma-separated values in `key` or `label`: `detail` "key(0): Too many values. Maximum supported is 5", `pos` 0. A `PUT` with another content type than
  JSON is `415`. `$select` with a field that does not exist leaves the field out (only unknown fields: items of `{}`). A number as `value` is stored as text.
  A `PUT` or `DELETE` of a locked key is `409` even when its `If-Match` would fail. A second snapshot of a name is `409` with `type` `.../already-exists`,
  `title` "The resource already exists.", and an empty `detail`.
- Every response carries `Sync-Token` (`zAJw6V16=<base64>;sn=<increasing>`), `x-ms-request-id`, `x-ms-correlation-request-id`,
  `x-ms-date`, `Strict-Transport-Security`, `Access-Control-Allow-Origin` and the echoed `x-ms-client-request-id`.
- Snapshots: `PUT` answers 201 `provisioning` (size 0, 7-digit `created`) with `Operation-Location` and
  `Link: </kv?snapshot=NAME&api-version=V>; rel="items"`. The snapshot is `ready` at once and the operation endpoint
  answers `Succeeded` at once. Settings are picked at creation (shared with the live store, not copied). `PATCH {"status": "archived"|"ready"}` changes the etag
  and sets or clears `expires`. Archived snapshots disappear after `retention_period`.

## Limits

Everything is in memory, so each store is bounded. Azure's numbers come from Microsoft Learn (FAQ: tiers; "Snapshots in
Azure App Configuration"; REST API pages for key-values and revisions); the defaults are the emulator's choice where the tiers differ.

| What | Azure | Here (default) | Setting |
|---|---|---|---|
| Revision age | 7 days (Free, Developer), 30 days (Standard, Premium) | 30 days, counted from the revision's own time; the current setting is never dropped | `AZEMU_APPCONFIG_REVISION_DAYS` |
| Revisions per key and label | no number | 1000 (the oldest go) | `AZEMU_APPCONFIG_REVISIONS_PER_KEY` |
| Bytes per store (settings and history) | 10 MB, 500 MB, 1 GB, 4 GB by tier | 100 MB: history goes oldest first; a write that would make the current settings alone too big is `429 too-many-requests` (policy `Storage Quota`) | `AZEMU_APPCONFIG_STORE_MB` |
| Bytes of all snapshots per store | 10 MB, 500 MB, 1 GB, 4 GB by tier | 50 MB, else `429` (policy `Snapshot Storage Quota`) | `AZEMU_APPCONFIG_SNAPSHOT_MB` |
| One snapshot | 1 MB | 1,000,000 bytes, else `400 invalid-argument` (the status is a guess: the page gives no error) | fixed |
| Snapshots per store | no number | 10,000, else `429` (policy `Snapshot Count`; a guess) | `AZEMU_APPCONFIG_SNAPSHOTS` |
| Comma-separated values in a `key` / `label` filter, `tags` filters | 5 | 5, else `400 invalid-argument` ("key(0): Too many values. Maximum supported is 5", as Azure says); also in snapshot filters | fixed |
| Items per page | 100 | 100 on every list; a `Range` read on `/revisions` gets one page too (`items 0-99/total`) | fixed |

How it is kept small: a snapshot holds the same stored settings as the live store (a pointer per item, not a copy), so a snapshot of unchanged
settings costs almost nothing and one of changed settings costs what the old values take (counted against the snapshot quota, plus a little per
item). Revisions are indexed by write order, by key and by time, so retention costs what it removes. A list looks at most 200,000 items (plus
one unit per tag when tag filters are used) per request while it holds the lock of the service; when a page is not full by then the answer is what
was found and the `Link` header continues after the last item looked at (so such a page can have fewer than 100 items and still link on). Reads
with `Accept-Datetime` look through the history (bounded by the store limit). The lock is still one for all stores.

## Known differences from Azure

- A ready snapshot has `size` of at least 1000 (also with no item), as recorded. `x-ms-item-count` counts only this store's items.

- No throttling: the recordings contain 431 `429 too-many-requests` answers; the emulator never sends one.
- The answers above were tried against a free-tier store in November 2026 (24 requests in this fix, about 75 before); the rest of what is written here as "recorded" comes
  from the recordings. The lock before `If-Match` was tried for `PUT`; `DELETE` is given the same order by analogy.
- Revision order inside one second follows write order; real Azure sorts those by etag.
- `Range` on `/revisions` (206 / 416 with `Content-Range`): checked against Azure in November 2026 (a 206 has `Content-Range`, the page `ETag`, and
  `Link` and `@nextLink` with the `after` cursor when more follow).
- Assumptions not verifiable from the recordings: lock and unlock create a revision and a new etag (the etag change is recorded,
  the revision is not; tried in November 2026: both make a revision and a new etag); `DELETE` with `If-Match` on a
  missing setting is 412 (tried: also this one); a snapshot filter without `label` selects the null label only; default snapshot retention is 30 days;
  snapshot `size` is the byte total rounded up to 1000 (recorded: 1000 for one small setting).
- An `after` cursor the service did not issue starts from the first page instead of failing.
- Not enforced: key and value size (Azure: 10 KB per key-value including its attributes; here only the 1 MiB request limit; the free-tier store
  answered `413` with an nginx page for a value of 12,000 bytes and took 9,000: that limit comes from the front end and may depend on the tier,
  so it is not copied), 429 for request rates. Only `.` and `..` are rejected as keys. What is enforced is under "Limits".
- `Accept` is ignored; the content type is always the recorded one. The label `""` is the null label.

## Keeping data between runs (`--data-dir`)

`<data-dir>/appconfig/appconfig.state` (kind `appconfig`, version 1; `crates/services/appconfig/src/persist.rs`): for each store every kept revision of every
key-value (key, label, value, content type, description, tags, lock flag, ETag, time, the time it was replaced or deleted), which revision is the current one for
each (key, label), the revision counter, and the snapshots with the settings they hold (settings shared between revisions and snapshots are shared again after
the load). The indexes (by key, by age, labels, byte counts) are built again. The limits (`AZEMU_APPCONFIG_*`) come from the new run, not from the file. The
`Sync-Token` counter is not kept. Reads are not changes; old revisions are dropped when something looks at the store, as before.

## Reset, state file, unsupported list

`POST /_azemu/reset?account=<store>` empties one store (settings, history, snapshots); persistence sees a change. The settings `AZEMU_APPCONFIG_*` are read through `azemu_core::settings` and described in `docs/SETTINGS.md`. Version 1 of `appconfig.state` is checked in under `crates/services/appconfig/tests/state-v1/appconfig/` and loaded by `tests/state_v1.rs`. A 501 is named `AppConfig_Unknown (GET /{}/{})` in `GET /_azemu/unsupported` (fixed words of the path stay, names of keys and snapshots are `{}`).
