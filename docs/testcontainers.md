# Using azemu from Testcontainers

Testcontainers starts a container from inside a test, tells the test which port on the host the container's port has, and removes the container when the test is done. This page
shows how to do that with azemu in Rust, Go and Python, for the three kinds of client a test usually needs: Blob, Queue and Table over plain http, services that need https and a token
(Key Vault, App Configuration, Cosmos DB, and every client of the Rust SDK), and Service Bus over AMQP.

Every code block below is a piece of a small project that was run: [`examples/testcontainers/rust`](../examples/testcontainers/rust/README.md),
[`go`](../examples/testcontainers/go/README.md) and [`python`](../examples/testcontainers/python/README.md). Copy the project, not the pieces. What was run, and what was not, is listed
at the end. Nothing here needs a package of azemu's own: the library is the Testcontainers library of your language.

| | Rust | Go | Python |
|---|---|---|---|
| Testcontainers | `testcontainers` 0.28.0 (feature `http_wait_plain`) | `testcontainers-go` 0.45.0 | `testcontainers` 4.15.0 |
| Clients | `azure_storage_blob` 1.1.0, `azure_security_keyvault_secrets` 1.0.1, `azure_identity` 1.0.0, `azservicebus` 0.25.1 | `azblob` 1.8.3, `azsecrets` 1.5.1, `azidentity` 1.15.0, `azservicebus` 1.11.0 | `azure-storage-blob` 12.31.1, `azure-keyvault-secrets` 4.12.0, `azure-identity` 1.27.0, `azure-servicebus` 7.15.0 |
| Language | Rust 1.99 | Go 1.27 | Python 3.13 |

All of it ran against `ghcr.io/tirthpatell/azemu:0.2.0` with Docker 29.10 on Linux x86-64 (a virtual machine with 8 cores), and the three example projects ran again against the 0.3.0 image (built from this source, before it was published). What the page says about timing is from that machine, with the 0.2.0 image unless it says otherwise.

**Contents:** [1. What every language needs](#1-what-every-language-needs) · [2. Start the container](#2-start-the-container) · [3. Plain http](#3-plain-http-with-a-connection-string) ·
[4. https with a token](#4-https-with-a-token-credential) · [5. Service Bus](#5-service-bus) · [6. Reset, seed, share](#6-reset-seed-file-and-sharing-one-container) ·
[7. Cleaning up](#7-cleaning-up) · [8. Other languages](#8-other-languages) · [9. What was run](#9-what-was-run-and-what-was-not)

## 1. What every language needs

Three things: the image, the ports a test uses, and a wait until the emulator answers.

- **The image, with a version.** `ghcr.io/tirthpatell/azemu:0.3.0`. Below 1.0 a minor version can change a flag or a setting, so do not use `latest` in a test suite.
- **The ports.** The image starts with `--host 0.0.0.0 --standard-ports`: the port picks the service, and any host name works. A test lists the container ports it needs and asks the
  library which host port each got; Docker chooses a free one, so nothing is fixed and parallel test runs do not collide.

  | Container port | What it is |
  |---|---|
  | 4577 | the gateway: `/_azemu/health`, `/_azemu/ca.pem`, `/_azemu/reset`, `/_azemu/identity/msi/token` (every other port answers `/_azemu/` paths too) |
  | 10000, 10001, 10002 | Blob, Queue, Table (the Azurite ports; the examples use 10000) |
  | 10010, 10011, 10012 | Key Vault, App Configuration, Event Grid (https; the examples use 10010) |
  | 8081 | Cosmos DB (https) |
  | 5672, 5671 | Service Bus and Event Hubs, AMQP plain and inside TLS (the examples use 5672) |

  The whole list is in [`reaching-the-emulator.md`](reaching-the-emulator.md#standard-ports).
- **A wait on `GET /_azemu/health`.** It answers `200` as soon as the emulator is listening. The container is not ready before that, and a test that starts too early gets a refused connection.

## 2. Start the container

Rust (`tests/common/mod.rs`; the crate's `with_mapped_port` with host port 0 means "any free port", and a port that is not listed is not published):

```rust
        let mut request = GenericImage::new(IMAGE, TAG)
            // Ready when the gateway answers 200 on /_azemu/health.
            .with_wait_for(WaitFor::http(
                HttpWaitStrategy::new("/_azemu/health").with_port(GATEWAY.tcp()).with_expected_status_code(200u16),
            ))
            // A seed file: copied into the container (a bind mount would need the file on the Docker host).
            .with_copy_to("/seed.json", seed.as_path())
            .with_env_var("AZEMU_SEED", "/seed.json")
            // The emulator has no sign-in: publish only on the loopback address (the default is every address).
            // Remove this for a Docker host on another machine.
            .with_host_config_modifier(|host_config| {
                for bindings in host_config.port_bindings.iter_mut().flat_map(|map| map.values_mut()) {
                    for binding in bindings.iter_mut().flatten() {
                        binding.host_ip = Some("127.0.0.1".to_string());
                    }
                }
            });
```

```rust
        // Host port 0: Docker picks a free one. A port that is listed here is published; the others are not.
        for port in [GATEWAY, BLOB, KEY_VAULT, SERVICE_BUS] {
            let host_port = if fixed_service_bus_port && port == SERVICE_BUS { SERVICE_BUS } else { 0 };
            request = request.with_mapped_port(host_port, port.tcp());
        }
        let container = request.start().await.expect("start the azemu container");
```

Go (`emulator/emulator.go`):

```go
	ctr, err := testcontainers.Run(ctx, Image,
		testcontainers.WithExposedPorts(ports...),
		testcontainers.WithWaitStrategy(wait.ForHTTP("/_azemu/health").WithPort(Gateway).WithStatusCodeMatcher(func(s int) bool { return s == 200 })),
		// A seed file: copied into the container (a bind mount would need the file on the Docker host).
		testcontainers.WithFiles(testcontainers.ContainerFile{HostFilePath: seedFile, ContainerFilePath: "/seed.json", FileMode: 0o644}),
		testcontainers.WithEnv(env),
		// The emulator has no sign-in: publish only on the loopback address (the default is every address).
		// Remove this for a Docker host on another machine.
		testcontainers.WithHostConfigModifier(func(hc *container.HostConfig) {
			hc.PortBindings = network.PortMap{}
			for _, p := range ports {
				hc.PortBindings[network.MustParsePort(p)] = []network.PortBinding{{HostIP: netip.MustParseAddr("127.0.0.1"), HostPort: "0"}}
			}
		}),
	)
```

Python (`conftest.py`):

```python
    container = DockerContainer(IMAGE)
    for port in (GATEWAY, BLOB, KEY_VAULT, SERVICE_BUS):
        # The emulator has no sign-in: publish only on the loopback address (the default is every address).
        # Remove the address for a Docker host on another machine.
        container.ports[str(port)] = ("127.0.0.1", None)
    container.with_env("AZEMU_SEED", "/seed.json")
    if name := os.environ.get("AZEMU_TLS_NAME"):  # for a Docker host that is not "localhost": see the page
        container.with_env("AZEMU_TLS_NAME", name)
    # A seed file: copied into the container (a bind mount would need the file on the Docker host).
    container.with_copy_into_container(Path(__file__).parent / "seed.json", "/seed.json")
    # Ready when the gateway answers 200 on /_azemu/health.
    container.waiting_for(HttpWaitStrategy(GATEWAY, "/_azemu/health").for_status_code(200))
```

**Ports are open to the network unless you say otherwise.** The libraries publish a container's port on every address of the Docker host (`0.0.0.0` and `[::]` in `docker ps`: seen with Go and Python; for Rust it is in the crate's source, which leaves the address out), and the emulator has no sign-in
([`SECURITY.md`](../SECURITY.md)). On a laptop or a shared build machine that makes the emulator reachable from the network for as long as the test runs. The examples publish on `127.0.0.1` only (the lines above:
the Python one sets the address in the port table, the Go and Rust ones change the container's host configuration after the library has made it). Tried in all three: `docker ps` showed
`127.0.0.1:<port>` for every published port, and the tests passed. Take these lines out when the Docker daemon is on another machine (section 4).

**The image's own health check as the wait.** It works in all three libraries (`WaitFor::healthcheck()`, `wait.ForHealthCheck()`, `HealthcheckWaitStrategy()`). With the 0.2.0 image it was slow: the container
became healthy about 3.2 seconds after it started in every run, where the wait on `/_azemu/health` returned after 0.25 to 0.35 seconds. From 0.3.0 the image checks every 250 ms during its first 10 seconds
(`--start-interval=250ms --start-period=10s`): a container of the 0.3.0 image started with `docker run` was healthy 0.84 seconds later (one measurement, with Docker itself; not measured through the three libraries). The wait on the
path is still the fastest. The start interval needs Docker Engine 25.0 or newer (its documentation says so); the Dockerfile line is refused by a builder older than that (`unknown flag: start-interval`), and a daemon older than that was
not tried here: it has no such field, so it should skip it (it is a JSON field of the image configuration), and the check would be as slow as the 0.2.0 image's.

**How long a start takes.** From the call that starts the container until the wait returns, the wait on `/_azemu/health`, after the image was pulled:

| | Runs | Start until ready | Start, ready and removal |
|---|---|---|---|
| Rust | 8 | 0.23 to 0.34 s (the first one 1.03 s) | 0.51 to 0.63 s |
| Go | 5 | 0.31 to 0.36 s (the first one 0.67 s) | not measured |
| Python | 4 | 0.22 to 0.28 s (the first one 0.46 s) | 0.41 to 0.46 s |
| any of them, wait on the image's health check, 0.2.0 image | 3 to 4 each | 3.1 to 3.8 s | |

The first start of a run is slower: it does work that happens once (connecting to Docker and, in Go and Python, starting the reaper container, section 7). The image is 61 MB; pulling it took about 2 seconds here.

## 3. Plain http with a connection string

Blob, Queue and Table accept the Azurite connection string, so a test needs no certificate and no token. Put the published ports into it. Go (`blob_test.go`):

```go
	conn := "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;" +
		"AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;" +
		"BlobEndpoint=" + e.URL("http", emulator.Blob, "/devstoreaccount1") + ";"
	client, err := azblob.NewClientFromConnectionString(conn, nil)
```

Python (`test_blob.py`):

```python
    connection_string = (
        f"DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey={ACCOUNT_KEY};"
        f"BlobEndpoint={emulator.url('http', BLOB, '/devstoreaccount1')};"
    )
    service = BlobServiceClient.from_connection_string(connection_string)
```

The account key is the well-known development key, the same as Azurite's. Queue and Table work the same way: add `QueueEndpoint=http://<host>:<port of 10001>/devstoreaccount1;` and `TableEndpoint=http://<host>:<port of 10002>/devstoreaccount1;`
(and publish those two ports). Tried with the Python SDKs in a test that is not in the example: a queue message sent and received, and a table entity written and read.

**Rust has no connection string.** The Rust Blob and Queue crates take a URL and a token credential; they have no Shared Key and no connection-string parser. A client built with no credential may use plain http, and
then it needs a SAS in the URL, signed with the development key. The example builds one (`dev_sas`, in `tests/common/mod.rs`; [`rust.md`](rust.md#5-build-each-client) explains it) and gives it to the client:

```rust
        let url = Url::parse(&format!("http://{}:{port}/{account}?{}", self.host, dev_sas())).unwrap();
        BlobServiceClient::new(url, None, None).unwrap()
```

The emulator checks that signature only for `devstoreaccount1`; any other account name is new and empty, and its SAS is not checked. The Rust Table crate is the 2024 `azure_data_tables`; it was not tried with Testcontainers.

## 4. https with a token credential

Key Vault, App Configuration, Cosmos DB and Event Grid are https services, and the Azure SDKs refuse to send a token over plain http: the Rust, Go and Python Key Vault clients all do. Four things have to be right.

**1. The certificate authority is new in every container.** The emulator makes one at every start, so a test fetches `ca.pem` from the container it started (`GET /_azemu/ca.pem` on any port)
and makes **one client** trust it. Do not set `SSL_CERT_FILE` or `REQUESTS_CA_BUNDLE` for the whole test process: those replace the system's list, and every other https call from it then fails.

Go: a transport with its own root pool, given to the client (`emulator/emulator.go`, and `keyvault_test.go` for the client):

```go
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(pem) {
		return nil, fmt.Errorf("ca.pem holds no certificate")
	}
	return &http.Client{Transport: &http.Transport{TLSClientConfig: &tls.Config{RootCAs: pool, MinVersion: tls.VersionTLS12}}}, nil
```

```go
	secrets, err := azsecrets.NewClient(e.URL("https", emulator.KeyVault, ""), credential, &azsecrets.ClientOptions{
		ClientOptions: azcore.ClientOptions{Transport: trusting(t, e)},
```

Python: the SDK clients take a file (`conftest.py` saves it, `test_key_vault.py` uses it):

```python
    secrets = SecretClient(
        emulator.url("https", KEY_VAULT),
        ManagedIdentityCredential(),
        connection_verify=emulator.ca_file(tmp_path),
```

Rust: a `reqwest` client that has the certificate added, given to the Blob, Queue or Key Vault client as its transport (`tests/key_vault.rs`; use `reqwest` 0.13, the version the Azure crates use):

```rust
    let ca = reqwest::Certificate::from_pem(&emulator.ca_pem().await).unwrap();
    let http = reqwest::Client::builder().tls_certs_merge(vec![ca]).build().unwrap();
```

```rust
        client_options: ClientOptions {
            transport: Some(Transport::new(Arc::new(http))),
            retry: RetryOptions::none(),
            ..Default::default()
        },
```

The Rust Cosmos DB and Event Hubs clients have no such option: they read only `SSL_CERT_FILE`, which is per process ([`rust.md`](rust.md#3-reach-it)). That does not fit a test that starts a new container, and with a new authority, in each test; it was not tried.

**2. The token comes from the emulator.** A service on Azure uses the managed identity credential; two variables send it to the emulator's token endpoint, on the gateway port of this container. Go (`keyvault_test.go`):

```go
	t.Setenv("IDENTITY_ENDPOINT", e.URL("http", emulator.Gateway, "/_azemu/identity/msi/token"))
	t.Setenv("IDENTITY_HEADER", "azemu")
	credential, err := azidentity.NewManagedIdentityCredential(nil)
```

Python (`test_key_vault.py`):

```python
    monkeypatch.setenv("IDENTITY_ENDPOINT", emulator.url("http", GATEWAY, "/_azemu/identity/msi/token"))
    monkeypatch.setenv("IDENTITY_HEADER", "azemu")
```

Rust (`tests/key_vault.rs`):

```rust
    unsafe {
        std::env::set_var("IDENTITY_ENDPOINT", format!("http://{}:{gateway}/_azemu/identity/msi/token", emulator.host));
        std::env::set_var("IDENTITY_HEADER", "azemu");
    }
    let credential = ManagedIdentityCredential::new(None).unwrap();
```

The credential reads the variables when it is made, and the port is different for every container. A variable is shared by the whole process, so tests that run at the same time in one process must not set it to different
values: Go's `t.Setenv` and pytest's `monkeypatch` undo it and the tests do not run in parallel; Rust's `set_var` is `unsafe` since edition 2024, and the example keeps it safe by having one test in that test file (cargo runs the files one after the other). Any other credential works too, because
the emulator accepts any token (unless it runs with `--strict-auth`: [`strict-auth.md`](strict-auth.md)).

**3. The Key Vault challenge.** Key Vault clients refuse a challenge whose resource is not the vault's own domain, and a host with one label (`localhost`) fails that check. Switch it off on the client. Tried without the switch, Go: the
request failed with `challenge resource "https://vault.azure.net" doesn't match the requested domain`; Python: `ValueError: The challenge resource 'vault.azure.net' does not match the requested domain`. The Rust crate's
message for it is in [`rust.md`](rust.md#5-build-each-client) (tried there against `https://localhost:10010`, not in this example). With the switch:

Go: `DisableChallengeResourceVerification: true,` in `azsecrets.ClientOptions`. Python: `verify_challenge_resource=False,` in `SecretClient(...)`. Rust:

```rust
        verify_challenge_resource: Some(false),
```

**4. The host name is part of the address.** The certificate the emulator makes covers `localhost`, anything under `.localhost` and the loopback addresses. With Testcontainers on the same machine the host is `localhost`, and that is enough.
On Key Vault, App Configuration, Event Grid and Cosmos DB the host name is also the vault, store, topic or account: `localhost` is the vault `default`, which is why `seed.json` of the examples has its secret under `default`.

**When the Docker daemon is on another machine** (a remote Docker host in CI), the host is not `localhost`, and this changes:

- The container's ports must be published on an address the test can reach: take out the lines that bind to `127.0.0.1` (above), and keep the network closed in another way (a private network, or a firewall).
- The client has to connect by a **name**, and that name goes into the certificate: start the container with `AZEMU_TLS_NAME=<name>` (a name or a suffix; `ci.internal` allows `docker-host.ci.internal`). The examples pass the variable on when it is set
  in the test's environment. In Go the library's own `TESTCONTAINERS_HOST_OVERRIDE` makes `Host()` return that name. Without it the emulator refuses the handshake, logs one warning that names the host and the setting, and the client sees only a TLS error.
- **A client that connects to an IP address** (a Docker host reached by its address, CI with a remote daemon) sends no host name, so the emulator cannot pick a certificate by name. List the address in `AZEMU_TLS_NAME=<address>`:
  the certificate served when there is no name then also names that address (as an IP alternative name), and the client accepts it once it trusts `ca.pem`. Only single addresses are put in the certificate, not a range
  (`10.0.0.0/24` lets the CA sign for the range, but an address in it that was not listed alone gets a certificate that does not name it). With the 0.2.0 image this did not work: the certificate had `localhost`, `127.0.0.1`
  and `::1` only, and `curl` answered `no alternative certificate subject name matches target ipv4 address`. Tried with a 0.3.0 binary: `curl --cacert ca.pem https://<the machine's address>:<port>/_azemu/health` on a binary started
  with `--tls-name <that address>` answered; not tried with a container or with a remote Docker daemon, and not with the Rust, Go or Python libraries. An address from the public internet is allowed (you listed it) and logs a warning at start:
  do not publish the emulator's ports there (it has no sign-in), and do not install `ca.pem` system-wide.
- The host name is now the vault, the account, the namespace: seed and create under that name, not `default`.

Tried in Go, with Docker on the same machine: `TESTCONTAINERS_HOST_OVERRIDE` set to a name that resolves to the machine, the ports published on every address, and the seed's vault renamed to that name. Without `AZEMU_TLS_NAME` the Key Vault test failed after
about 10 seconds (`remote error: tls: access denied`; the SDK retries); with it the Blob, Key Vault and Service Bus tests passed. **Not run:** Rust and Python against a Docker daemon on another machine or with a host name other than `localhost`, and a real remote Docker host in any language.

**Not run with Testcontainers:** Cosmos DB, App Configuration and Event Grid. They need the same four things, and [`reaching-the-emulator.md`](reaching-the-emulator.md#https-and-the-emulators-certificate-authority) says what each SDK needs for
the certificate. One is a difference that matters for a test: the Python Cosmos client ignores a CA file in `connection_verify`, so it needs `connection_verify=False`.

## 5. Service Bus

AMQP is not http, so there is no certificate to trust: the clients use the plain AMQP port (5672) with `UseDevelopmentEmulator=true`, which turns TLS off and keeps the port of the connection string. The host `localhost` is the namespace `default`.
The connection string, Go (`servicebus_test.go`):

```go
	conn := "Endpoint=sb://" + e.Host + ":" + e.Port(emulator.Bus) + ";SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true"
```

Python (`test_service_bus.py`):

```python
    connection_string = (
        f"Endpoint=sb://{emulator.host}:{emulator.port(SERVICE_BUS)};"
        "SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true"
    )
```

A queue exists only after something creates it, as on Azure. The Go and Python administration clients ignore `UseDevelopmentEmulator` and always speak https to the same host and port, so they need the CA of section 4:

```go
	adm, err := admin.NewClientFromConnectionString(conn, &admin.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: trusting(t, e)}})
```

```python
    admin = ServiceBusAdministrationClient.from_connection_string(connection_string, connection_verify=emulator.ca_file(tmp_path))
    admin.create_queue("commands")
```

(`AZEMU_SERVICEBUS_AUTOCREATE=1` in the container's environment makes queues appear when they are first used; it would not be like Azure, and it was not tried here.)

**Rust.** There is no official Service Bus crate; the example uses the unofficial `azservicebus` 0.25.1, with its `unsecured` feature (plain AMQP). It builds the address from the host name alone and **dials port 5672 whatever the connection string says**
(read from its source, and seen: with a published port other than 5672 the test failed with `Connection refused`). So the Rust test publishes the container's 5672 on the host's 5672, the one place where the examples fix a port, and no other program on the machine may hold it while the test runs.
This is the only crate that has the problem here; the Go and Python clients keep the port. With a TLS client the same crate dials 5671, so the same holds there (not tried with Testcontainers). The queue is made with the HTTP call that the administration clients make, which also works over plain http on this port
(`tests/service_bus.rs`):

```rust
    let response = reqwest::Client::new()
        .put(format!("http://{}:{port}/commands?api-version=2021-05", emulator.host))
        .header("Authorization", "Bearer any")
        .header("Content-Type", "application/atom+xml")
        .body(entry)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
```

```rust
    let connection = format!("Endpoint=sb://{}:{port};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any", emulator.host);
    let mut client =
        ServiceBusClient::unsecured().new_from_connection_string(connection, ServiceBusClientOptions::default()).await.unwrap();
```

The fixed port is the `fixed_service_bus_port` branch of the loop in section 2.

## 6. Reset, seed file and sharing one container

**Reset.** `POST /_azemu/reset` on the gateway port empties every service; `?account=<name>` empties one account in every service that has accounts, and leaves the others. The helpers of the examples have both (`Reset()` in Go, `reset()` and `reset_account()` in Rust, `reset()` in Python).

**A seed file.** `AZEMU_SEED` names a JSON file in the container: the blob containers, queues, tables, Cosmos databases and containers, secrets and settings that must exist before a test starts
([`seed.md`](seed.md)). It is made at start and again after every reset, so a test that resets still finds them. Copy the file into the container before it starts (the three libraries do, shown in section 2): a bind mount would need the file on the
machine where Docker runs. The example's file:

```json
{
  "blob": { "devstoreaccount1": { "containers": ["uploads"] } },
  "keyvault": { "default": { "secrets": { "db-password": "local-only-password" } } }
}
```

Tried in all three: the Blob test uploads into `uploads` without creating it, the Key Vault test reads `db-password`, writes `api-key`, resets, and then finds `api-key` gone and `db-password` back.

**One container per test, or one for the whole run.**

| | One container per test | One container for the whole run |
|---|---|---|
| Cost | 0.25 to 0.35 s to start and 0.2 to 0.3 s to remove, for every test | paid once |
| Isolation | complete: nothing is left over, and a test may change settings or add fault rules ([`faults.md`](faults.md)) | by reset, or by a separate account name for each test |
| Where it is in the examples | the root of the Go and Python projects (`scope="function"` in Python), `tests/blob.rs`, `key_vault.rs` and `service_bus.rs` in Rust | `go/shared/`, `conftest.py` (the default scope, `session`), `tests/shared.rs` |
| Parallel tests | each has its own ports | share it only with an account of their own each, and `reset?account=` after, never the full reset |

A suite of a hundred tests pays about a minute for a container each; a few tests do not notice. Reset between tests is the same call in every language; with a shared container, run tests that use it one after another, or give each its own account.

- **Go:** one container in `TestMain`, a reset in `t.Cleanup` (`shared/shared_test.go`). Tried: with the reset left out, the second test finds the first one's blob and fails; with it, it passes.
- **Python:** a fixture with `scope="session"` and an autouse fixture that resets after each test (`conftest.py`). Tried with `scope="function"` too, one container for each of the three tests: all pass, in 5.5 s instead of 3.1 s.
- **Rust:** the tests of one test file run in parallel, so each uses its own account name and empties only that (`tests/shared.rs`, with `OnceCell`). **A container in a `static` is never dropped, so it stays after the tests**: the crate has no reaper process (section 7).
  Tried: two tests sharing a container left it running after `cargo test` ended, until it was removed by hand. The example removes it with a function that runs when the program exits (`dtor` crate, `#[dtor::dtor(unsafe)]`, which calls `docker rm -f`); tried: the container was gone afterwards.

## 7. Cleaning up

A container is removed when the value that started it is dropped (Rust), when `Terminate` is called (Go, and `t.Cleanup` does it), or when the `with` block ends (Python). If the test program is killed, it is the **reaper** that matters. Go and Python start a small
container, `testcontainers/ryuk`, that removes what the program started when the program's connection to it ends. Tried with `kill -9` on a program that had started an azemu container: both removed it within 12 seconds. **The Rust crate has no reaper**
(it has a `watchdog` feature for SIGINT, SIGTERM and SIGQUIT, which was not tried). Tried: the same `kill -9` left the container running. Remove such containers by hand: `docker ps --filter ancestor=ghcr.io/tirthpatell/azemu:0.3.0`.
The Docker daemon of a CI job that is thrown away afterwards needs none of this.

## 8. Other languages

Any other Testcontainers library needs the same three things: the image, the mapped ports, and a wait on `GET /_azemu/health`. The rest of this page (the certificate authority, the identity variables, the Key Vault switch, `AZEMU_TLS_NAME`, the seed file, the reset)
is about the emulator and the Azure SDKs, not about the library. It was not tried with other libraries.

## 9. What was run, and what was not

**Run** (`examples/testcontainers`, against the published image 0.2.0, the three projects one after the other): Rust `cargo test`, five tests, 8 s with the build already done; Go `go test ./...`, five tests, 3.1 s; Python `pytest`, three tests, 3.6 s.
The same three runs passed again against the 0.3.0 image, which is the version the examples now name.
Each project has a Blob test over plain http, a Key Vault test over https with the managed identity credential and the certificate authority of its own container, and a Service Bus send, receive and complete. Go and Rust also have a test file or package
with a shared container (Python's fixture is shared already). Also run: the timings above, the health-check wait in all three, loopback publishing in all three, `kill -9` in all three, the Key Vault test without the challenge switch (Go, Python), and
`AZEMU_TLS_NAME` with a host name that is not `localhost` (Go). The first Rust build compiles the Azure crates: about a minute with two jobs.

**Not run:** Cosmos DB, App Configuration and Event Grid with Testcontainers; Queue and Table in the examples (tried in Python in a separate test); a Docker daemon on another machine; Docker Desktop, Podman, Windows and macOS; the Rust `watchdog` feature; the Rust Table crate; Service Bus over TLS or WebSocket
through Testcontainers; `pytest-xdist` or several test processes at once.
