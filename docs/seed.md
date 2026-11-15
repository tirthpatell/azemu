# The seed file

On Azure, the blob containers, queues, tables, Cosmos DB databases and containers, secrets and settings that an application uses are made by infrastructure code
(Terraform, Bicep, a pipeline) before the application runs. Against the emulator nothing exists until something makes it. A seed file is a list of those things:
the emulator makes them when it starts, and again after every reset, so a test does not need a set-up step of its own.

```bash
azemu --seed seed.json
AZEMU_SEED=/seed.json azemu          # the same; in a container, see below
```

## An example

This file uses every kind of item the format has:

```json
{
  "blob": {
    "devstoreaccount1": {
      "containers": [
        "uploads",
        { "name": "public-files", "publicAccess": "blob", "metadata": { "team": "red" } }
      ]
    }
  },
  "queue": { "devstoreaccount1": { "queues": ["jobs", "jobs-poison"] } },
  "table": { "devstoreaccount1": { "tables": ["Orders"] } },
  "cosmos": {
    "myaccount": {
      "databases": [
        {
          "id": "app",
          "containers": [
            { "id": "items", "partitionKey": "/tenantId", "defaultTtl": -1,
              "uniqueKeyPolicy": { "uniqueKeys": [{ "paths": ["/email"] }] } },
            { "id": "events", "partitionKey": ["/tenantId", "/userId"] }
          ]
        }
      ]
    }
  },
  "keyvault": { "myvault": { "secrets": { "db-password": "local-only-password" } } },
  "appconfig": {
    "mystore": {
      "keyValues": [
        { "key": "App:Title", "value": "Hello" },
        { "key": "App:Title", "label": "dev", "value": "Hello from dev", "contentType": "text/plain" }
      ]
    }
  }
}
```

The names under a service (`devstoreaccount1`, `myaccount`, `myvault`, `mystore`) are the account, vault or store: the first part of the host name the client uses
(`myvault.vault.localhost`), or the name in the path on a pinned port. Any name works; nothing has to be made first. A client that reaches a service by `localhost` or an
address on a standard port uses the service's default account: `devstoreaccount1` for Blob, Queue and Table, `default` for Key Vault and App Configuration, `localhost` for Cosmos.
With the host name `azemu` (Compose) the vault, the store and the Cosmos account are called `azemu`.

## What the file may hold

The top level has one property per service. Each holds the accounts, vaults or stores, and under each one property that lists the items.

| Service | Property | Items |
|---|---|---|
| `blob` | `containers` | a name, or `{ "name", "publicAccess", "metadata" }`. `publicAccess` is `blob` or `container`. `metadata` is names and string values. |
| `queue` | `queues` | names |
| `table` | `tables` | names |
| `cosmos` | `databases` | an id, or `{ "id", "containers" }`. A container is `{ "id", "partitionKey" }`. `partitionKey` is required: a path (`"/tenantId"`), or a list of up to three paths for a hierarchical key. The properties `uniqueKeyPolicy`, `defaultTtl`, `indexingPolicy`, `conflictResolutionPolicy`, `geospatialConfig`, `computedProperties`, `analyticalStorageTtl`, `changeFeedPolicy`, `vectorEmbeddingPolicy`, `fullTextPolicy` and `clientEncryptionPolicy` are passed to the service as they are written. |
| `keyvault` | `secrets` | `{ "secret-name": "value" }` |
| `appconfig` | `keyValues` | `{ "key", "value" }` and, optionally, `label` and `contentType`. A feature flag is a key-value with its own key prefix and content type. |

Things the file does not hold: the contents of blobs, queue messages, table rows, Cosmos documents, Key Vault keys and certificates, throughput settings. Make those
in the test.

**Service Bus, Event Hubs and Event Grid** keep their own files, which work as before and are not part of the seed file: `AZEMU_SERVICEBUS_CONFIG`,
`AZEMU_EVENTHUBS_CONFIG` and `AZEMU_EVENTGRID_CONFIG` (formats on [`servicebus.md`](services/servicebus.md), [`eventhubs.md`](services/eventhubs.md) and
[`eventgrid.md`](services/eventgrid.md)). A seed file that has one of those three names as a property is refused, and the message names the variable to use.

## How the things are made

Each item is made by sending the service the same request that an SDK sends (create container, create queue, create table, create database, create container,
set secret, set key-value), inside the emulator. So the seed cannot make something that a client could not, the rules for names and policies are the service's own,
and what a client sees is what it would see after creating the item itself.

- **Something that already exists is left as it is.** That is not an error. It covers state read from `--data-dir` and a name that is in the file twice (the first
  one counts). A secret or key-value that exists is not set again, so it does not get a second version or revision.
- **Order.** Items are made in the order of the file. A database comes before its containers.
- **Before the ready line.** The emulator makes the items before it listens, so a client that waits for the ready line, or for `GET /_azemu/health`, finds them.
- **Where the file is.** A relative path is relative to the directory the program is started in. `GET /_azemu/config` shows the file in force as an absolute path
  under `seedFile` (`null` when there is none).

## Reset

`POST /_azemu/reset` empties every service and makes the seed's items again. A test that created other things finds them gone and the seed back.

- `?service=queue` makes again only the queues. `?service=dfs` (Data Lake, which shares its state with Blob) makes the blob containers again.
- `?account=myacct` makes again only what the file lists under that account, vault or store name, in the services that the reset emptied.
- Both together: the services and the account both count.
- `?save=true` saves after the seed is made.
- `RunningEmulator::reset()` and the test server of `azemu-core` do the same as the admin call.

A failure after a reset (it worked at start, so it should not happen) is logged at `ERROR` with the same message as at start. The reset itself still counts and answers
as it always does.

## With `--data-dir`

The state is read from the directory first, then the seed makes what is missing. What the seed made is saved with the rest. At the next start those things exist and are left as they are:
a container whose metadata a test changed keeps the change, a secret with a new version keeps it. Taking an item out of the file does not remove it from the directory;
a thing that was deleted is made again at the next start.

## A file that is wrong

The start is refused (exit code 1, nothing listens) and the message names the file, the place in it and the reason:

```text
Error: seed file /work/seed.json: blob.devstoreaccount1.containers[1]: the blob service refused it: 400 InvalidResourceName: The specifed resource name contains invalid characters.
Error: seed file /work/seed.json: queue.devstoreaccount1.quues: unknown property (known here: queues)
Error: seed file /work/seed.json: bolb: unknown top-level property (known: blob, queue, table, cosmos, keyvault, appconfig)
Error: seed file /work/seed.json: not valid JSON: expected value at line 1 column 32
Error: seed file seed.json: cannot read it: No such file or directory (os error 2)
```

An unknown property is an error, so a typing mistake does not pass quietly. A program that has not every service (the `azemu-blob` binary, for example) refuses a file that
lists a service it does not have.

## In a container

Mount the file and name it with the variable:

```bash
docker run -v ./seed.json:/seed.json:ro -e AZEMU_SEED=/seed.json -p 127.0.0.1:4577:4577 ghcr.io/tirthpatell/azemu:latest
```

The file must exist on the host before Docker starts: for a path that does not exist Docker makes an empty directory, and the emulator then says it is a directory.
The user in the image (id 10001) must be able to read the file. [`examples/seed`](../examples/seed/README.md) is a Compose project that does this (its page says whether it was run;
the `docker run` line above was not).

## In a Rust test

`azemu-testkit` passes arguments to the program it starts, so `AZEMU_TESTKIT_ARGS="--seed tests/seed.json"` should give every test program a seeded emulator (not tried; the path is
relative to where the program is started). For things that only some tests need, `create_blob_container`,
`create_cosmos_database` and `create_cosmos_container` make one item from the test itself (see [`rust.md`](rust.md#7-create-what-a-test-needs)).

## Choices that could not be checked against Azure

Azure has no seed file; these are choices, not copies.

- A secret that was deleted but can still be recovered counts as existing, so it is not set again (setting it would be refused).
- A container that is in the file twice, with different settings: the first one stays, the second is ignored without a message.
- `partitionKey` is required for a Cosmos container, as the SDKs and Terraform require it. The service itself would take none and use `/_partitionKey`.
- The partition key made from a path or a list of paths is always version 2 (`Hash`, or `MultiHash` for more than one path), what the current SDKs make.
- A key-value with an empty `label` has no label.
- After a reset, the first item a service refuses ends the seeding; the items after it are not made.

## What was tried

The Rust tests in `crates/core` (reading and checking the file, the order and scope of what is made again, what counts as existing) and `crates/azemu/tests/seed.rs` (the
real services at start, after a reset and a partial reset, with `--data-dir` across a restart, every refusal above, a failure after a reset, the program with a
relative path and with the variable); the Python SDK suite `tests/sdk-python/seed` (a container, a queue, a table, a Cosmos container, a secret and a key-value
found with the official SDKs); the program itself for the messages above and for `GET /_azemu/config`; `curl` for the Cosmos calls on the [Cosmos page](services/cosmos.md).
