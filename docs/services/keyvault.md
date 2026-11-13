# Key Vault (`keyvault`)

**What it emulates.** The Azure Key Vault data-plane REST API: secrets, keys (with real cryptography: RSA, EC, sign, verify, wrap, encrypt, import,
backup, restore) and certificates (self-signed, import, PEM and PKCS#12, policies), soft delete and purge, versions, paging. Crate `azemu-keyvault`,
standalone binary `azemu-keyvault`.

**How to reach it.** `https://{vault}.vault.localhost:4577` (also `http://` for tools that do not need the SDK's https rule); any vault name works and is
made on first use. With `--standard-ports`: `https://azemu:10010`, where the vault is the host name (`localhost` or an address: the vault `default`).
The SDKs insist on https, so the client must trust the emulator's certificate authority (`docs/reaching-the-emulator.md`); any bearer token is accepted, a request
without one gets the 401 challenge the SDKs need. The SDKs' challenge check passes without a switch for the gateway form; a host with a single label
(`azemu:10010`, the standard-port form from a container) still needs the client's switch (`verify_challenge_resource=False` and its twins; see below).

**Not built.** Key release, key attestation, secure wrap/unwrap, symmetric key operations (symmetric keys can be created, imported, listed and backed up, not
used), managed storage accounts, Managed HSM administration (RBAC, backup/restore of a whole HSM), the Security Domain API. `-HSM` key types are accepted but
the key material is software. No RBAC, access policies, firewall or private endpoints. Certificate issuance finishes at once (Azure takes seconds); auto-renew
and key rotation policies are stored, not run.

| | |
|---|---|
| Service name | `keyvault` |
| Gateway URL | `https://{vault}.vault.localhost:{port}/secrets/...` (also `http://`, same port) |
| Default vault on a pinned port | `default` (only when the host has no `{vault}.vault` label) |
| Vaults | any name works without being created first; every vault is separate |
| State | in memory, and on disk too with `--data-dir`; `POST /_azemu/reset` drops everything, `POST /_azemu/reset?account=<vault>` one vault (secrets, keys, certificates, their deleted copies) |
| `api-version` | required; every `7.x`, `7.x-preview.y`, date style (`2016-10-01`, `2025-07-01`, `2026-04-01`, `2026-01-01-preview`) is accepted |

## Pointing the Azure SDKs at it

The SDKs insist on HTTPS and on a challenge handshake. The emulator answers both; you
only have to trust its CA and use any credential, because any `Authorization: Bearer <token>`
is accepted. The SDKs' check of the challenge resource needs no switch any more (see "The challenge resource" below).

```
azemu                       # gateway on 4577, or: azemu-keyvault --port 4580
curl http://localhost:4577/_azemu/ca.pem -o ca.pem   # or ~/.azemu/ca.pem
```

```python
from azure.core.credentials import AccessToken
from azure.keyvault.secrets import SecretClient
from azure.keyvault.keys import KeyClient
from azure.keyvault.certificates import CertificateClient

class FakeCredential:
    def get_token(self, *scopes, **kwargs):
        return AccessToken("any-token", 4102444800)

url = "https://myvault.vault.localhost:4577"      # "myvault" is created on first use
kw = dict(connection_verify="ca.pem")
secrets = SecretClient(url, FakeCredential(), **kw)
keys = KeyClient(url, FakeCredential(), **kw)
certs = CertificateClient(url, FakeCredential(), **kw)
secrets.set_secret("db-password", "hunter2")
```

A name under `.localhost` resolves on a Linux desktop only. Elsewhere use `--standard-ports` (and the switch above), a hosts-file line for
`myvault.vault.localhost`, or `--resolve` with curl. The same works over plain HTTP for tools that do not need the SDK's HTTPS rule.

Other languages need the same: a way to trust a CA file.

### The challenge resource

A request without `Authorization` gets `401` with
`WWW-Authenticate: Bearer authorization="https://login.microsoftonline.com/<tenant>", resource="..."`. Every client (secrets, keys,
certificates, Managed HSM paths: the same challenge) compares the `resource` with the host it called and refuses the answer unless the
host ends with a dot and the resource's host. Azure's `resource` is `https://vault.azure.net`, which no `*.vault.localhost` host ends with,
so a client would need its switch (`verify_challenge_resource=False`, `DisableChallengeResourceVerification = true`,
`.disableChallengeResourceVerification()`, `disableChallengeResourceVerification: true`, Rust `verify_challenge_resource: Some(false)`).

The emulator now names the request's own domain: the host without its first label, with the port.
`myvault.vault.localhost:4577` gets `resource="https://vault.localhost:4577"`. A host under a real Azure suffix (`vault.azure.net`,
`vault.azure.cn`, `vault.usgovcloudapi.net`, `managedhsm.azure.net` and the like: a hosts-file set-up) keeps `https://vault.azure.net`.
Code: `challenge_resource` in `crates/services/keyvault/src/auth.rs`. What passes without the switch, tried with each SDK (the clients of
the test suites under `tests/`; Python, Go, JavaScript, .NET, Java, Rust):

| Host of the vault | Python | Go | JavaScript | .NET | Java | Rust |
|---|---|---|---|---|---|---|
| `myvault.vault.localhost:4577` (the gateway) | works | works | works | works | works | works |
| any name with two labels or more on a pinned or standard port, `sub.localhost:10010` | works | works | works | works | works | works |
| IPv4 address, `127.0.0.1:10010` (resource `https://0.0.1:10010`) | works | works | switch needed | switch needed | switch needed | switch needed |
| one label, `localhost:10010`, `azemu:10010` (the standard-port form), IPv6 literal | switch needed | switch needed | switch needed | switch needed | switch needed | switch needed |

Why: Python and Go compare the whole authority (`host:port`) as text, so the IPv4 form passes there. The others take the resource through a URL
parser, which reads `0.0.1` as the address `0.0.0.1` (JavaScript, .NET, Rust) or finds no host (Java), and they compare the host only. No name
can pass for a host with a single label: the check needs a dot in front of the resource's host. Those clients keep their switch; the emulator
answers them with `vault.azure.net` and the client's own error text names the switch. How this was established: the gateway form runs
in the test suites of all six languages. The pinned-port forms were run against an emulator with `--pin keyvault=<port>` for Python, Go, JavaScript
and .NET (`127.0.0.1`, `localhost`, `sub.localhost`). For Java and Rust the other rows are read from their check (`KeyVaultCredentialPolicy` in the
Java secrets jar, `authorizer.rs` of `azure_security_keyvault_secrets` 1.0.1), not run.
The tests under `tests/` that do not need the switch do not set it, one test per suite shows that it still works.

A request without `Authorization` gets the `401` above and `{"error":{"code":"Unauthorized","message":"AKV10000: Request is missing a Bearer, PoP, or MTLS_POP token."}}`
for every `api-version`, even when the request carries a body. (The recordings show that the wording changed in the service over time, not with the
`api-version`: before March 2026 it was `... a Bearer or PoP token.` for every version, later `7.4` and `7.5` recorded in October 2026 say the new
text too. The emulator answers as the service does now.)

## What `GET /_azemu/unsupported` shows

`KeyVault_ManagedStorage (GET)` for the managed storage account paths, `KeyVault_<operation> (symmetric key)` for crypto with an `oct` key, and
`KeyVault_Unknown (DELETE /keys/{}/rotationpolicy)` for a request no route matches (names of secrets, keys, certificates and versions are `{}`).

## Operations

### Secrets (all built)

| Operation | Route | Status |
|---|---|---|
| Set secret | `PUT /secrets/{name}` | built |
| Get secret (latest / version) | `GET /secrets/{name}[/{version}]` | built, `outContentType` for certificate secrets |
| Update properties | `PATCH /secrets/{name}[/{version}]` | built |
| Delete (soft) | `DELETE /secrets/{name}` | built |
| List / versions | `GET /secrets`, `GET /secrets/{name}/versions` | built (`maxresults`, `$skiptoken`, `nextLink`, 25 per page) |
| Deleted: list / get / recover / purge | `GET /deletedsecrets[/{name}]`, `POST .../recover`, `DELETE /deletedsecrets/{name}` | built |
| Backup / restore | `POST /secrets/{name}/backup`, `POST /secrets/restore` | built |

### Keys (all built except release, attestation and the secure operations)

| Operation | Route | Status |
|---|---|---|
| Create | `POST /keys/{name}/create` | RSA 2048/3072/4096, EC P-256/P-384/P-521/P-256K, `oct`/`oct-HSM` (material only, see below) |
| Import | `PUT /keys/{name}` | RSA, EC, oct from a JWK; public parts only in the answer |
| Get / update | `GET`, `PATCH /keys/{name}[/{version}]` | built (`key_ops`, tags, `enabled`, `nbf`, `exp`, release policy field) |
| Delete / deleted list / get / recover / purge | `/keys`, `/deletedkeys` | built |
| List / versions | `GET /keys`, `GET /keys/{name}/versions` | built |
| Backup / restore | `POST /keys/{name}/backup`, `POST /keys/restore` | built (private keys included) |
| Rotate | `POST /keys/{name}/rotate` | built |
| Rotation policy | `GET`, `PUT /keys/{name}/rotationpolicy` | built, merge rule copied from recordings |
| Random bytes | `POST /rng` | built (1 to 128 bytes) |
| sign / verify | `POST /keys/{name}/{version}/sign`, `.../verify` | RS256/384/512, PS256/384/512, ES256/384/512, ES256K, performed with the stored private key |
| encrypt / decrypt | `.../encrypt`, `.../decrypt` | RSA1_5, RSA-OAEP, RSA-OAEP-256 |
| wrapKey / unwrapKey | `.../wrapkey`, `.../unwrapkey` | RSA1_5, RSA-OAEP, RSA-OAEP-256 |
| Release | `.../release` | 501 `keyvault: POST ...` |
| Attestation | `GET .../attestation` | 501 |
| Secure wrap/unwrap | `.../securewrapkey`, `.../secureunwrapkey` | 501 |
| Operations with a symmetric (`oct`) key | encrypt, decrypt, wrap, sign, ... | 501 |

All cryptography is pure Rust (`rsa`, `p256`, `p384`, `p521`, `k256`, `sha1`, `sha2`): a signature the
emulator makes verifies with the downloaded public key in `cryptography`, OpenSSL and so on, and data
encrypted with the public key decrypts on the emulator. RSA signatures take a *digest* as the SDKs send
it (PKCS#1 v1.5 with the DigestInfo prefix, PSS with a salt as long as the digest). ECDSA signatures are
the raw `r || s` form (RFC 7518), base64url without padding; high-S signatures from other libraries
verify too.

### Certificates

| Operation | Route | Status |
|---|---|---|
| Create | `POST /certificates/{name}/create` | issuer `Self` (and any named issuer) is signed at once; issuer `Unknown` stays pending until merged; RSA 2048/3072/4096 and EC P-256/P-384/P-521/P-256K keys; `preserveCertOrder` echoed |
| Import | `POST /certificates/{name}/import` | `value` is base64 of a PEM bundle, base64 of a PKCS#12 file, or the PEM bundle as plain text (the JavaScript SDK's form; it sends no policy, so the secret's content type, PEM or PKCS#12, is taken from the value). PEM (key + certificates), PKCS#12 with or without password, including unencrypted and legacy-encrypted files; the policy is made from the certificate (issuer `Unknown` and an `EmailContacts` lifetime action at 80 % when the request has no policy) |
| Get / versions / list | `GET /certificates[/{name}[/{version}]]`, `.../versions` | built; `includePending=true` also lists certificates whose request has not produced a certificate yet |
| Update properties | `PATCH /certificates/{name}[/{version}]` | built |
| Policy | `GET`, `PATCH /certificates/{name}/policy` | built |
| Operation | `GET`, `PATCH`, `DELETE /certificates/{name}/pending` | built; deleting the operation of a request that produced no certificate leaves the certificate (policy only), which can still be deleted |
| Merge | `POST /certificates/{name}/pending/merge` | built; the certificate's public key must match the pending key |
| Delete / deleted list / get / recover / purge | `/certificates`, `/deletedcertificates` | built; the linked key and secret move with it; a pending certificate is deleted too (bundle with `id`, `attributes`, `policy`, no `cer`) |
| Backup / restore | `POST /certificates/{name}/backup`, `POST /certificates/restore` | built |
| Issuers | `/certificates/issuers[/{name}]` | built (`PUT`, `GET`, `PATCH`, `DELETE`, list) |
| Contacts | `/certificates/contacts` | built (`PUT`, `GET`, `DELETE`) |

Every certificate has a key and a secret of the same name and version (`kid`, `sid`, both marked
`managed`; the key carries the certificate's `nbf` and `exp`). The secret holds the PKCS#12 (base64, empty password) or the PEM bundle, depending on
`secret_props.contentType` of the policy; `GET /secrets/{name}?outContentType=application/x-pem-file`
converts PFX to PEM like the real service.

The certificate azemu issues for a self-signed policy has the profile of the ones Key Vault issues (read from the
recordings): version 3, 16 byte serial number, `sha256WithRSAEncryption` for RSA keys (ECDSA with SHA-256 / 384 / 512
for P-256 and P-256K / P-384 / P-521), issuer = subject, valid from ten minutes before the request for the policy's
`validity_months`, and the extensions key usage (critical), basic constraints (not critical), extended key usage,
subject alternative name (critical only when the subject is empty), authority key identifier and subject key
identifier (both the SHA-1 of the public key, so equal). Alternative names: `dns_names`, `emails`, `uris`,
`ipAddresses` and `upns` (an `otherName` with OID 1.3.6.1.4.1.311.20.2.3). A subject that is not an X.500
distinguished name (`CN=a, O=b`; the types CN, O, OU, C, L, S/ST, E, DC, STREET, SERIALNUMBER, T, SN, GN, I, UID or a
dotted OID) is refused with `400 BadParameter`; an empty subject is allowed. The certificate and the PKCS#10 request
are written as DER by `cert_build.rs` (no X.509 library) and signed with the same pure Rust code as the key operations.

Managed storage accounts (`/storage`, `/deletedstorage`) answer 501 `NotImplemented`. Any other operation
that is not built answers `501 {"error":{"code":"NotImplemented","message":"keyvault: <METHOD> <path>"}}`.

## What it stores and how it behaves

- Objects have versions (32 hex characters, random), oldest first; "latest" is the newest.
  Names are `[0-9a-zA-Z-]{1,127}` (`400 BadParameter` "The request URI contains an invalid name: ..."),
  looked up case-insensitively.
- Soft delete is always on: `recoveryLevel` `Recoverable+Purgeable`, `recoverableDays` 90. Deleting returns
  `recoveryId`, `deletedDate`, `scheduledPurgeDate`. A deleted name cannot be reused (`409 Conflict`,
  `ObjectIsDeletedButRecoverable`); recover brings back every version, purge removes it. Deleted objects
  are dropped for good when their purge date passes.
- Responses carry the optional attribute fields the requested `api-version` returned in the recordings:
  `recoverableDays` after 7.0, `exportable` from 7.4, `hsmPlatform` from 7.5, `key_size` (RSA keys) from
  the 2025 versions.
- A disabled secret answers `403 Forbidden` / `SecretDisabled` on get (also on get of the deleted secret, as
  recorded). Keys: `enabled`, `nbf`, `exp` and `key_ops` are enforced for every crypto operation
  (`403 Forbidden`, inner `KeyDisabled` / `KeyOperationForbidden`). Algorithm for the wrong key type or curve,
  digest of the wrong length, ciphertext that does not decrypt: `400 BadParameter`.
- Lists return at most 25 items per page (`maxresults` lowers it) sorted by name (versions by version id),
  with a `nextLink` built from the URL the client used.
- Key and certificate generation runs on a blocking thread, never under the state lock. In the debug build
  (the big-number crates are compiled with `opt-level = 3` in the root `Cargo.toml`) an RSA 2048 key takes
  well under a second and RSA 4096 under a second to a few seconds.
- Backups are `base64url(len + "AzureKeyVault{Secret|Key|Certificate}BackupV1.microsoft.com" + JSON)` with
  the private material inside. They restore into the same or another vault (ids point at the vault
  being restored into) and keep version ids and timestamps.

## Known differences from Azure

The replay's value comparison (`tools/replay`) found some wording that changed in the service over time: the 401 text, the RSA-HSM exponent, `hsmPlatform`; the emulator uses the newest recorded form.
- `GET /deletedsecrets|deletedkeys|deletedcertificates/{name}` for a name that is nowhere (never made or purged) answers the long `A secret with (name/id) ... was not found in this key vault` text (recorded for a purged name; inferred for a name never made); for a live name `Deleted Secret not found: X`. Azure also answers the short text while it is still finishing a purge, which the emulator never does (it purges at once).
- Certificate creation (issuer `Self`) takes seconds on Azure (`inProgress`, then `completed`; `cancelled` after a cancel); the emulator reports `completed` on the first poll. The preview `platformManaged` policy of API `2026-03-01-preview` is not built.

- **Java SDK defaults (not in any recording).** `public_exponent: 0` on a key create means "not given" (65537): the Java
  `createRsaKey` always sends it. An empty `key_ops` list on create or import means the default operations of the key type
  (the Java `importKey` sends `[]` for a key with no operations, and decrypt then answered 403). The recordings of the Python
  SDK always send explicit operations, so what Azure does with `[]` is inferred from the Java SDK's own use; update keeps an
  explicit list as sent. Tests: `tests/sdk-python/keyvault/test_kv_keys_java_sdk_requests.py`.
- **Timing.** Real Key Vault finishes soft delete, recover and purge asynchronously: `GET /deletedsecrets/{n}`
  answers 404 for a moment after a delete, and a purged name answers `409 ObjectIsBeingDeleted` for a while.
  The emulator finishes at once. A certificate create answers `202` with `status: inProgress` (the SDK pollers
  need that) and the first `GET .../pending` already says `completed`.
- **Missing `api-version`** answers `400 BadParameter` with the text "The api-version query parameter
  (?api-version=) is required for all requests." There is no recording of that answer; the wording is not
  verified. The same goes for the messages of the crypto errors, the oversized secret value (25 KiB limit)
  and the garbage-backup error, which are not in the recordings.
- Authorization is not checked beyond the scheme: `Authorization: Bearer <anything>` passes, anything else is
  challenged. There are no RBAC, access policies, firewall or private endpoints.
- Backup blobs are not encrypted and cannot be restored by Azure (nor Azure's by the emulator).
- A `$skiptoken` the emulator did not issue is ignored (first page again) instead of failing.
- `-HSM` key types (`RSA-HSM`, `EC-HSM`, `oct-HSM`) are accepted and echoed in `kty` (with `hsmPlatform` "2"),
  but the key material is software. Symmetric keys can be created, imported, listed and backed up, not used.
- Certificates: issuer names other than `Unknown` behave like `Self` (the issuer's provider is never
  contacted). A pending certificate (issuer `Unknown`) has no certificate version until merged, so
  `GET /certificates/{name}` answers 200 with a bundle that has no `cer`, `kid`, `sid` or `x5t` (it has `id`
  with a made-up version, `attributes`, `policy` and `pending`), and so does `GET /certificates/{name}/{that version}`
  (without `policy` and `pending`); any other version is 404. The JavaScript SDK reads this before the operation.
  `attributes.enabled` is true until the request is cancelled (the only recording of such a bundle is of a cancelled
  request, `enabled: false`; none exists of one in progress, so that is a guess). Such a certificate is listed with
  `includePending=true` (not in the plain list), its versions list has the one made-up version, and it can be deleted,
  recovered and purged. A cancelled request makes that bundle `enabled: false`; the real service
  also does this for a self-signed request, which azemu finishes at once and so cannot cancel. Auto-renew and the
  rotation of lifetime actions are stored, not executed (also for key rotation policies). `validity_months` of an
  imported certificate is its validity in months of 30.5 days (a twelfth of 365 days before 7.5), as recorded for
  one certificate; other values are extrapolated. The order of several alternative names inside one certificate,
  the encoding of subject attributes other than CN (PrintableString, or UTF8String when needed) and the signature
  hash of RSA keys larger than 2048 bits (SHA-256 always) are not in the recordings. Extension order and the
  PKCS#10 request (key usage, extended key usage, SAN, basic constraints) were checked against recorded certificates
  and requests of RSA 2048 keys only.
- The distinguished name check is a syntax check (known attribute type, non-empty value); the recordings have no
  rejected subject, so the exact rules and message of the real service are not verified.
- Access to `managed` keys and secrets is not restricted: you can update or delete the secret behind a
  certificate by itself.
- Not built: release, key attestation, secure wrap/unwrap, symmetric key operations, managed storage
  accounts, Administration (RBAC, backup/restore of a whole HSM) and Security Domain APIs.

## Keeping data between runs (`--data-dir`)

`<data-dir>/keyvault/keyvault.state` (kind `keyvault`, version 1; `crates/services/keyvault/src/persist.rs`): for each vault its secrets, keys and
certificates with every version (attributes, tags, content types, release policies, rotation policies, certificate policies, the certificate DER, the secret
and the linked key of a certificate), deleted items with their deleted and scheduled purge dates, pending certificate operations (with the private key of a
request that waits for its signed certificate), issuers and contacts. **Private key material and secret values are stored in the clear**; the directory is
mode 0700 and the file mode 0600 (set again at every start, in case it was copied with wider rights). RSA keys are stored as n, e, d, p, q; EC keys as the
curve and the private scalar; symmetric keys as their bytes; on load they are rebuilt and checked (a key that does not work is an error naming the file).
A signature or decryption made before a restart is therefore valid after it, and a deterministic signature (RS256) comes out the same.
Reads, key operations (sign, verify, encrypt, decrypt, wrap, unwrap), `rng` and backups are not changes. The `kid` of the secret behind a certificate is stored
as a path (`/keys/name/version`) and given the address of the request when it is returned, so it is right on whatever port a later run uses.
Version 1 of the file is checked in under `crates/services/keyvault/tests/state-v1/keyvault/` (a throwaway EC key, a throwaway imported RSA key and a
self-signed certificate made for the fixture) and loaded by `tests/state_v1.rs`; a change that breaks reading it fails that test and tells the author to
raise the version and keep a reader.
