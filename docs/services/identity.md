# Identity (token endpoints)

**What it emulates.** The token endpoints of Microsoft Entra ID and of managed identity, so that the Azure Identity libraries (`DefaultAzureCredential`,
`ManagedIdentityCredential`, `ClientSecretCredential`, `WorkloadIdentityCredential`, device code, password and authorization code flows) get their
tokens from the emulator instead of Entra ID, and application code written for real Azure runs unchanged. The tokens are real RS256 JWTs; the key set is
served at the usual `discovery/keys` paths.

**How to reach it.** Managed identity, no code change: `IDENTITY_ENDPOINT=http://localhost:4577/_azemu/identity/msi/token` and `IDENTITY_HEADER=azemu`
(`docs/services/identity.md`, "Using it from the Azure Identity libraries"). Entra style: the authority `https://login.localhost:4577/{tenant}`
or `https://localhost:4577/_azemu/identity/login/{tenant}`. With `--standard-ports`: `http://azemu:10014/{tenant}/oauth2/v2.0/token`. A client that
sends a bearer token over `https` needs the emulator's certificate authority (`docs/reaching-the-emulator.md`).

**Not built.** Nothing is validated: any secret, assertion, password or certificate is accepted, and no registry of tenants, applications or managed
identities exists. The other services do not check the tokens either. Older managed identity endpoints (`MSI_ENDPOINT`, Azure Arc, Service Fabric, Cloud
Shell), `form_post`, PKCE, consent and logout answer 501 or are ignored. Tokens are version 1.0 style and carry no `roles`.

Crate `azemu-identity` (`crates/services/identity`). `name()` is `identity`. Mounts:
`Host("login")`, `PathPrefix("/_azemu/identity")`, `PathPrefix("/metadata/identity")`. No account.
The only state is the RS256 signing key (RSA-2048, pure Rust `rsa`, generated at start on a blocking
thread, kept across `POST /_azemu/reset`). Refresh tokens, device codes and authorization codes are
sealed into the opaque string itself, so nothing else is stored; the consequence is that codes are
not single use and still work after a restart.

## Operations

| Operation | Status |
|---|---|
| `GET /_azemu/identity/msi/token?resource=&api-version=2019-08-01[&client_id=\|&object_id=\|&principal_id=\|&mi_res_id=]`, header `X-IDENTITY-HEADER` (App Service style) | built |
| `GET /metadata/identity/oauth2/token?api-version=2018-02-01&resource=[&client_id=\|&object_id=\|&msi_res_id=]`, header `Metadata: true` (IMDS). Without the header: 400 `bad_request_102`, which is what azure-identity's probe gets | built |
| `GET /{tenant}/v2.0/.well-known/openid-configuration` and the v1 path | built |
| `GET /common/discovery/instance` (MSAL instance discovery) | built |
| `GET /{tenant}/discovery/v2.0/keys`, `/{tenant}/discovery/keys`, `/common/discovery/keys` | built |
| `POST /{tenant}/oauth2/v2.0/token` and v1 `/{tenant}/oauth2/token` (`resource=`): `client_credentials` (secret, HTTP Basic, or `client_assertion`), `refresh_token`, `password`, `authorization_code`, `urn:ietf:params:oauth:grant-type:device_code` and the short `device_code` (msal-go and msal-node send that), `urn:ietf:params:oauth:grant-type:jwt-bearer` (on-behalf-of) | built |
| `POST /{tenant}/oauth2/v2.0/devicecode` (and v1), approved immediately; `GET /{tenant}/oauth2/deviceauth` (a static page) | built |
| `GET /{tenant}/oauth2/v2.0/authorize` (and v1): signs the user in at once and redirects to `redirect_uri` with `code` and `state` in the query. `response_mode=query` and `response_type=code` only (others redirect with an `error`) | built |
| `GET /common/userrealm/{user}` | built |
| `GET /_azemu/identity/token?resource=` (or `scope=`; optional `client_id`, `tenant`, `expires_in`) returns the token JSON, for curl and scripts | built |
| anything else under the mounts (`/metadata/identity/info`, `/oauth2/v2.0/logout`, `userinfo`, ...) | 501 `NotImplemented` naming the operation |

Every Entra path also exists under `/_azemu/identity/login/{tenant}/...`, so an authority is either
`https://login.localhost:{port}/{tenant}` or `https://localhost:{port}/_azemu/identity/login/{tenant}`.
Any tenant id or name is accepted (names with characters outside `A-Za-z0-9._-` get Entra's error 90002).
Unknown grant: `unsupported_grant_type` in Entra's error JSON (`error`, `error_description`, `error_codes`,
`timestamp`, `trace_id`, `correlation_id`).

## Tokens

RS256 JWTs. Header: `typ`, `alg`, `kid`, `x5t`. Claims are the version 1.0 form Entra issues for Azure
resources (Storage, Key Vault, ARM) whichever endpoint the client used: `aud`, `iss`
(`https://sts.windows.net/{tid}/`), `iat`, `nbf`, `exp`, `appid`, `appidacr`, `idp`, `oid`, `sub`, `tid`,
`ver`, and `idtyp: app` for app-only tokens. User tokens (device code, password, code, on-behalf-of)
instead carry `upn`, `unique_name`, `name`, `scp`, `amr`, `acr`, and a pairwise `sub`; ID tokens are 2.0
tokens with `preferred_username`, `nonce`. `roles` is absent.

- `aud` comes from the scope (`https://storage.azure.com/.default` gives `https://storage.azure.com`) or
  from `resource=` / the managed identity `resource` as sent (a trailing slash stays).
- `oid` and `sub` are stable per client id; `appidacr` is `0` (public client), `1` (secret), `2`
  (certificate, assertion, managed identity). A managed identity requested by object id gets that object
  id as `oid`. A GUID tenant is the `tid` as is; `common`, `organizations` and `consumers` map to a default
  tenant, any other name to a stable GUID.
- Lifetime 1 hour. `?expires_in=N` (1 second to 7 days) is read on the managed identity endpoints, the helper
  and the query string of the token URL. MSAL's managed identity client treats a token with under 5 minutes
  left as expired, so very short lifetimes make every call fetch a new token.
- The v1 discovery document's issuer is `https://sts.windows.net/{tid}/`, which is what these tokens carry;
  the v2 document's issuer is `{authority}/{tid}/v2.0`, which is what the ID tokens carry. A resource server
  that validates access tokens against the v2 document would reject them (real Entra behaves the same for
  v1-token resources).

## Using it from the Azure Identity libraries

Python rows were run (azure-identity 1.27.0, msal 1.39.0). Rows for other languages are NOT tested: they are
what the libraries' public documentation says and must be checked before relying on them.

### Without a code change: managed identity

| Where | Settings |
|---|---|
| App Service style (any language that supports it) | `IDENTITY_ENDPOINT=http://localhost:{port}/_azemu/identity/msi/token` and `IDENTITY_HEADER=azemu`. Plain HTTP works. A user-assigned identity is chosen as in Azure: `client_id=` in the constructor, `managed_identity_client_id=` / `AZURE_CLIENT_ID` for `DefaultAzureCredential`, or object id / resource id. Python: tested for `ManagedIdentityCredential` and `DefaultAzureCredential`, sync and async. |
| IMDS (VM style) | Python: `AZURE_POD_IDENTITY_AUTHORITY_HOST=http://localhost:{port}`, tested for `ManagedIdentityCredential` and `DefaultAzureCredential`, sync and async. The other libraries read the same variable according to their docs (untested); without it they use `169.254.169.254`, which the emulator does not claim. |

### Client credentials and the other Entra flows

These need HTTPS (MSAL refuses an `http` authority) and instance discovery off, so there is a small
code or configuration change.

| Credential | What works (Python, tested) |
|---|---|
| `ClientSecretCredential(tenant, client, secret, authority="https://login.localhost:{port}", disable_instance_discovery=True, connection_verify=ca)` | yes; the same with `authority=https://localhost:{port}/_azemu/identity/login`; any tenant name; any secret |
| `EnvironmentCredential` / `DefaultAzureCredential` | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_AUTHORITY_HOST=https://login.localhost:{port}` plus `disable_instance_discovery=True` (constructor only: there is no environment variable for it). The CA can come from `REQUESTS_CA_BUNDLE` |
| `WorkloadIdentityCredential` | constructor (`tenant_id`, `client_id`, `token_file_path`) or `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST`; any file content is accepted as the federated assertion |
| `CertificateCredential` (also via `AZURE_CLIENT_CERTIFICATE_PATH`) | yes; the assertion is not verified |
| `DeviceCodeCredential` | yes; the code is approved at once, the prompt callback still runs |
| `AuthorizationCodeCredential` | yes, with a code from the emulator's `authorize` endpoint |
| `OnBehalfOfCredential` | yes; the user assertion is read without verifying its signature |
| `UsernamePasswordCredential` | only when the request MSAL makes to `https://login.localhost/common/userrealm/...` (port dropped, see below) reaches the emulator, i.e. a forward from port 443. Tested with a transport that adds the port |
| `InteractiveBrowserCredential` | not tested; the authorize endpoint signs in without a page, but the library's local redirect listener was not exercised |
| Async twins (`azure.identity.aio`) | tested: managed identity (App Service and IMDS), `DefaultAzureCredential`, `ClientSecretCredential`, `WorkloadIdentityCredential` |

### Other languages (untested, from the libraries' documentation)

| Language | Authority and instance discovery | CA | Managed identity |
|---|---|---|---|
| .NET (Azure.Identity) | `AZURE_AUTHORITY_HOST` or `options.AuthorityHost`; `DisableInstanceDiscovery = true` | install `ca.pem` in the OS store, or `SSL_CERT_FILE` on Linux | `IDENTITY_ENDPOINT` / `IDENTITY_HEADER`; `AZURE_POD_IDENTITY_AUTHORITY_HOST` for IMDS |
| Java (azure-identity) | `AZURE_AUTHORITY_HOST` or `.authorityHost(...)`; `.disableInstanceDiscovery()` on the builders | add the CA to the JVM trust store (`-Djavax.net.ssl.trustStore=...`) | `IDENTITY_ENDPOINT` / `IDENTITY_HEADER` |
| JavaScript (@azure/identity) | `AZURE_AUTHORITY_HOST` or `authorityHost`; `disableInstanceDiscovery: true` | `NODE_EXTRA_CA_CERTS=ca.pem` | `IDENTITY_ENDPOINT` / `IDENTITY_HEADER`; `AZURE_POD_IDENTITY_AUTHORITY_HOST` for IMDS |
| Go (azidentity) | `AZURE_AUTHORITY_HOST` or `ClientOptions.Cloud.ActiveDirectoryAuthorityHost`; `DisableInstanceDiscovery: true` | `SSL_CERT_FILE` | `IDENTITY_ENDPOINT` / `IDENTITY_HEADER`; `AZURE_POD_IDENTITY_AUTHORITY_HOST` for IMDS |

## HTTPS and the CA

Every listener answers HTTP and HTTPS on the same port. MSAL requires HTTPS for the authority, so the
authority is `https://login.localhost:{port}` (the CA mints a certificate for any SNI name) or
`https://localhost:{port}/_azemu/identity/login`. `login.localhost` resolved to the loopback address on the Linux machine the tests ran on (other systems untested: add a hosts entry or use the `localhost` form). Trust `~/.azemu/ca.pem`
(the `ca=` path on the startup line): Python `connection_verify=` per credential or `REQUESTS_CA_BUNDLE`.
Without it the credential fails with `CERTIFICATE_VERIFY_FAILED` (tested). For the real host name
`login.microsoftonline.com`, add a hosts-file entry to 127.0.0.1 and run the emulator on port 443; MSAL
then treats the authority as well-known and skips instance discovery (untested: it needs root or a port forward and a trusted CA).

## Instance discovery left on (observed)

MSAL does not know `login.localhost`, so unless `disable_instance_discovery=True` it validates the authority
first. What actually happens (Python, run on a machine with internet access):

1. MSAL's first request is `GET https://login.microsoftonline.com/common/discovery/instance?...` with the
   authority in the query. It is always that host, not the emulator, and the authority host it sends has no
   port. The emulator's `/common/discovery/instance` is not used.
2. With the system CAs the call reaches real Entra, which rejects the host; azure-identity raises
   `ClientAuthenticationError: The authority provided, https://login.localhost:{port}, is not well-known. ...
   passing in 'disable_instance_discovery=True'`.
3. With `connection_verify=<emulator CA>` the same call fails with `CERTIFICATE_VERIFY_FAILED`, because the
   custom CA is applied to every host, including login.microsoftonline.com.
4. Offline it fails with a connection error. So the SDK tests never depend on the internet: they use a
   transport (`idutil.RoutingTransport`) that refuses every non-local host.
5. If `login.microsoftonline.com` reaches the emulator (hosts entry), the emulator's endpoint answers, and its
   `tenant_discovery_endpoint` and the discovery document send MSAL on to the emulator's token endpoint;
   this works (test `test_instance_discovery_on_uses_our_endpoint_when_the_microsoft_host_maps_to_the_emulator`
   reroutes the host in the transport to show it).

The discovery answer lists the emulator host (without port) as its own network; the discovery and token
URLs keep the port.

## Known differences and limits

- Nothing is validated: any secret, assertion, password, certificate or federated token file is accepted as
  long as it is present; the emulator has no registry of tenants, applications or managed identities
  (every client id, object id or resource id is "known").
- `X-IDENTITY-HEADER` must be present but its value is not compared. The App Service error body for a
  missing header (`{"statusCode":400,"message":...}`) is modelled on the documented format, not on a recording.
- Only api-version `2019-08-01` is accepted on the App Service style endpoint (the older `MSI_ENDPOINT` /
  `2017-09-01` form and Azure Arc, Service Fabric, Cloud Shell, Azure ML endpoints are not built).
- Tokens are always version 1.0 style and carry no `x5c`, `aio`, `uti` or `roles`; the key set has `n`, `e`,
  `kid`, `x5t`, no certificate chain. Only one RSA key; it changes on every start unless `--data-dir` is set (clients that cache the key
  set across restarts see failures until they refetch).
- **`scope` of a token answer.** Every grant that signs a user in (password, refresh token, authorization code, device
  code, on-behalf-of) answers with a `scope` that holds what was granted (`.default` stands for `user_impersonation`), the
  `.default` scope as it was asked for, every other requested scope as written, and the OpenID scopes asked for. MSAL for Go
  refuses an answer that leaves out a scope it asked for (`token response failed because declined scopes are present`), and it
  asks for `{resource}/.default`. Client credentials answers have no `scope`, as Entra's have (MSAL then takes what it asked for).
  v1 answers have no `scope`. Entra's own answer to a `.default` request lists the `.default` entry too; the rest of the
  string is not compared with a recording.
- Device codes are approved without a user step; `authorization_pending` and `slow_down` never happen. The first
  poll gets the token, so a `DeviceCodeCredential` finishes without a person (msal-go polls after 50 ms, msal-node and
  Python MSAL poll at once and then every `interval`, 5 seconds, which a pending answer first would add).
  Authorization and refresh tokens do not expire and can be redeemed more than once.
- `authorize` only does `response_type=code` with `response_mode=query`; no PKCE check, no consent, no
  `form_post`.
- MSAL drops the port of the authority in instance discovery and in user realm discovery
  (`/common/userrealm/{user}`), so `UsernamePasswordCredential` needs a port-443 forward (see above).
- Other services do not validate these tokens (they accept any bearer token). A clean way to add that later: a
  small trait in core (`TokenValidator::validate(token) -> Result<Claims>`) implemented by `IdentityService`
  (it already has `signing_key()`), handed to services by the `azemu` binary. Not built.
- The service is mounted by the `azemu` binary (`azemu::full_stack()`); `azemu-identity` also runs on its own.

## Keeping data between runs (`--data-dir`)

The only state worth keeping is the RS256 signing key: `<data-dir>/identity/identity.state` (kind `identity`, version 1; `crates/services/identity/src/persist.rs`),
the RSA-2048 private key as n, e, d, p, q. **It is a private key** (file mode 0600, directory 0700). With it a token issued before a restart still verifies against the
key set after it, and a refresh token or authorization code (they carry what they need) still works. The key is saved the next time the driver saves after it was made
(within the save interval); a process killed in that window makes a new key on the next start. A key file that is not a working 2048-bit key is an error naming the file.

## Other notes

Identity has no accounts: `POST /_azemu/reset?account=` is not supported by it (the default). Version 1 of `identity.state` (the signing key) is checked in under `crates/services/identity/tests/state-v1/identity/` with the key set it served (`jwks.json`); the key in it is a throwaway made for the fixture. A 501 is named `Identity_Unknown (GET /metadata/identity/{}/{}/{})` in `GET /_azemu/unsupported`.
