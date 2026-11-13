#!/usr/bin/env python3
"""Run the `azurerm` Terraform provider against azemu, one case at a time.

For each case under tests/terraform/cases/ the runner

  1. starts target/debug/azemu on a free port with a temporary --cert-dir,
  2. runs `terraform init`, `apply`, `plan -detailed-exitcode` (it must report no changes), checks with the data plane SDKs
     that the things exist, and `terraform destroy`,
  3. prints one line per step, stops the emulator and removes every temporary file.

It needs the network for `terraform init` (the provider download), so it is not part of tests/run_sdk_suites.py.
The provider is cached in $TF_PLUGIN_CACHE_DIR (default refs/terraform-plugin-cache, which git ignores) and pinned by
tests/terraform/.terraform.lock.hcl. Run one runner at a time: two terraform inits on one cache directory collide.

TLS: ca.pem is a root that signs only for localhost names. Terraform needs one file with the system roots plus that CA;
it is built here and given as SSL_CERT_FILE to the terraform commands that talk to the emulator (not to `init`, which
talks to the real registry).

    .venv/bin/python tests/terraform/run.py                  # every case
    .venv/bin/python tests/terraform/run.py base eventgrid   # some cases
    .venv/bin/python tests/terraform/run.py --keep base      # keep the work directory (state, logs) and print it
    .venv/bin/python tests/terraform/run.py --bin target/debug/azemu --list

Exit code 0 when every step of every case passed.
"""

import argparse
import json
import os
import pathlib
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
CASES_DIR = HERE / "cases"
SUB = "11111111-2222-3333-4444-555555555555"

# Seconds a terraform command may take. Key Vault and storage account creation wait in the provider (see arm.md).
TIMEOUTS = {"init": 300, "apply": 900, "plan": 300, "destroy": 900, "output": 60}


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Emulator:
    """target/debug/azemu on a free port with a temporary cert dir."""

    def __init__(self, binary: pathlib.Path, workdir: pathlib.Path):
        self.port = free_port()
        self.cert_dir = workdir / "certs"
        self.log_path = workdir / "azemu.log"
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            [str(binary), "--port", str(self.port), "--cert-dir", str(self.cert_dir), "--log-level", "info"],
            stdout=self._log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"azemu exited early with code {self.proc.returncode}; see {self.log_path}")
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/_azemu/health", timeout=1) as r:
                    if r.status == 200:
                        break
            except Exception:
                time.sleep(0.1)
        else:
            self.stop()
            raise RuntimeError("azemu did not become healthy")
        self.ca_file = self.cert_dir / "ca.pem"

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self._log.close()

    # -- URLs the data plane SDKs use ------------------------------------------------------------------------------
    def url(self, label: str, account: str, scheme: str = "http") -> str:
        suffix = "core.localhost" if label in ("blob", "queue", "table", "dfs") else "localhost"
        return f"{scheme}://{account}.{label}.{suffix}:{self.port}"

    def get_json(self, path: str):
        with urllib.request.urlopen(f"http://127.0.0.1:{self.port}{path}", timeout=10) as r:
            return json.load(r)


def system_ca_file() -> str:
    paths = ssl.get_default_verify_paths()
    for candidate in (paths.cafile, paths.openssl_cafile, "/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt"):
        if candidate and os.path.exists(candidate):
            return candidate
    raise RuntimeError("no system CA bundle found")


def build_bundle(emu: Emulator, workdir: pathlib.Path) -> pathlib.Path:
    """The system roots plus the emulator's CA in one file (SSL_CERT_FILE replaces the system list)."""
    bundle = workdir / "ca-bundle.pem"
    bundle.write_bytes(pathlib.Path(system_ca_file()).read_bytes() + b"\n" + emu.ca_file.read_bytes())
    return bundle


class Terraform:
    def __init__(self, emu: Emulator, case_dir: pathlib.Path, bundle: pathlib.Path, cache: pathlib.Path):
        self.emu = emu
        self.dir = case_dir
        self.bundle = bundle
        self.cache = cache
        self.last_output = ""

    def run(self, step: str, args: list, *, trust_emulator: bool, ok_codes=(0,), timeout=None) -> int:
        env = dict(os.environ)
        env.update({"TF_IN_AUTOMATION": "1", "CHECKPOINT_DISABLE": "1", "TF_PLUGIN_CACHE_DIR": str(self.cache), "TF_INPUT": "0"})
        env.pop("SSL_CERT_FILE", None)
        if trust_emulator:
            env["SSL_CERT_FILE"] = str(self.bundle)
        cmd = ["terraform", *args]
        log = open(self.dir / f"{step}.log", "wb")
        proc = subprocess.Popen(cmd, cwd=self.dir, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = proc.wait(timeout=timeout or TIMEOUTS.get(args[0], 600))
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            code = -1
        except BaseException:  # Ctrl-C and the like: stop terraform and its provider, then go on unwinding
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            raise
        finally:
            log.close()
        self.last_output = (self.dir / f"{step}.log").read_text(errors="replace")
        return code

    def outputs(self) -> dict:
        env = dict(os.environ, SSL_CERT_FILE=str(self.bundle), CHECKPOINT_DISABLE="1")
        r = subprocess.run(["terraform", "output", "-json"], cwd=self.dir, env=env, capture_output=True, text=True, timeout=TIMEOUTS["output"])
        if r.returncode != 0:
            raise RuntimeError(f"terraform output failed: {r.stderr[-400:]}")
        return {k: v["value"] for k, v in json.loads(r.stdout).items()}


def tail(text: str, n: int = 14) -> str:
    lines = [l for l in text.strip().splitlines() if l.strip()]
    return "\n".join("      " + l for l in lines[-n:])


# -- the checks: the data plane SDKs look at what Terraform made ------------------------------------------------------

def wait_until(fn, timeout=15, what="condition"):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what} (last: {last!r})")


class FakeCredential:
    """The emulator accepts any bearer token."""

    def get_token(self, *scopes, **kwargs):
        from azure.core.credentials import AccessToken

        return AccessToken("azemu-test-token", int(time.time()) + 3600)


def storage_clients(out: dict):
    from azure.core.credentials import AzureNamedKeyCredential
    from azure.data.tables import TableServiceClient
    from azure.storage.blob import BlobServiceClient
    from azure.storage.queue import QueueServiceClient

    key = out["key"]
    blobs = BlobServiceClient(out["blob_endpoint"], credential=key)
    queues = QueueServiceClient(out["queue_endpoint"], credential=key)
    tables = None
    if out.get("table_endpoint"):
        tables = TableServiceClient(out["table_endpoint"], credential=AzureNamedKeyCredential(out["account"], key))
    return blobs, queues, tables


def check_base(emu: Emulator, out: dict):
    assert out["blob_endpoint"] == emu.url("blob", out["account"]) + "/", out["blob_endpoint"]
    blobs, queues, tables = storage_clients(out)
    assert blobs.get_container_client("photos").get_container_properties().name == "photos"
    blob = blobs.get_blob_client("photos", "hello.txt")
    assert blob.download_blob().readall() == b"hello from terraform"
    assert blob.get_blob_properties().content_settings.content_type == "text/plain"
    assert sorted(q.name for q in queues.list_queues()) == ["jobs", "jobs2"]
    queues.get_queue_client("jobs").send_message("ping")
    assert [t.name for t in tables.list_tables()] == ["People"]


def check_servicebus(emu: Emulator, out: dict):
    from azure.servicebus import ServiceBusClient, ServiceBusMessage

    kw = {"retry_total": 1, "retry_backoff_factor": 0.1, "retry_backoff_max": 0.5}
    with ServiceBusClient.from_connection_string(out["connection_string"], **kw) as client:
        with client.get_queue_sender("orders") as sender:
            sender.send_messages(ServiceBusMessage("from terraform"))
        with client.get_queue_receiver("orders", max_wait_time=5) as receiver:
            msgs = receiver.receive_messages(max_message_count=1, max_wait_time=5)
            assert [str(m) for m in msgs] == ["from terraform"]
            receiver.complete_message(msgs[0])
        with client.get_topic_sender("events") as sender:
            sender.send_messages(ServiceBusMessage("red one", application_properties={"color": "red"}))
            sender.send_messages(ServiceBusMessage("blue one", application_properties={"color": "blue"}))
        with client.get_subscription_receiver("events", "reds", max_wait_time=5) as receiver:
            msgs = receiver.receive_messages(max_message_count=5, max_wait_time=3)
            # a new subscription keeps its $Default (true) rule on Azure too, so the blue message arrives as well
            assert "red one" in [str(m) for m in msgs]


def check_eventhubs(emu: Emulator, out: dict):
    import threading

    from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

    kw = {"retry_total": 1, "retry_backoff_factor": 0.1, "retry_backoff_max": 0.5}
    cs = out["connection_string"]
    with EventHubProducerClient.from_connection_string(cs, eventhub_name="hub1", **kw) as producer:
        assert len(producer.get_partition_ids()) == 2
        batch = producer.create_batch(partition_id="0")
        batch.add(EventData("from terraform"))
        producer.send_batch(batch)
    got = []
    consumer = EventHubConsumerClient.from_connection_string(cs, consumer_group="workers", eventhub_name="hub1", **kw)

    def on_event(ctx, event):
        if event is not None:
            got.append(event.body_as_str())
            consumer.close()

    t = threading.Thread(target=lambda: consumer.receive(on_event=on_event, partition_id="0", starting_position="-1"))
    t.start()
    t.join(timeout=20)
    consumer.close()
    assert got == ["from terraform"], got


def check_keyvault(emu: Emulator, out: dict):
    from azure.keyvault.secrets import SecretClient

    assert out["vault_uri"] == f"https://tfkv-vault-01.vault.localhost:{emu.port}/", out["vault_uri"]
    client = SecretClient(out["vault_uri"], FakeCredential(), verify_challenge_resource=False, connection_verify=str(emu.ca_file))
    client.set_secret("from-terraform", "v1")
    assert client.get_secret("from-terraform").value == "v1"


def check_appconfig(emu: Emulator, out: dict):
    from azure.appconfiguration import AzureAppConfigurationClient, ConfigurationSetting

    assert out["endpoint"].startswith("https://tf-appconfig-01.azconfig.localhost"), out["endpoint"]
    client = AzureAppConfigurationClient.from_connection_string(out["connection_string"], connection_verify=str(emu.ca_file))
    client.set_configuration_setting(ConfigurationSetting(key="k", value="v"))
    assert client.get_configuration_setting("k").value == "v"


def check_cosmos(emu: Emulator, out: dict):
    from azure.cosmos import CosmosClient
    from azure.cosmos.documents import SSLConfiguration

    client = CosmosClient(out["endpoint"], credential=out["key"], connection_verify=str(emu.ca_file), ssl_config=SSLConfiguration(), retry_total=1)
    container = client.get_database_client("appdb").get_container_client("items")
    container.create_item({"id": "a", "pk": "x", "n": 1})
    assert container.read_item("a", partition_key="x")["n"] == 1


def check_eventgrid(emu: Emulator, out: dict):
    import base64

    blobs, queues, _ = storage_clients(out)
    container = blobs.get_container_client("inbox")
    container.upload_blob("from-terraform.txt", b"x")
    for queue in ("direct", "viasystem"):
        qc = queues.get_queue_client(queue)
        events = []

        def poll():
            for m in qc.receive_messages(max_messages=10):
                events.append(json.loads(base64.b64decode(m.content)))
                qc.delete_message(m)
            return events

        wait_until(poll, 15, f"an event in queue {queue}")
        ev = events[0]
        assert ev["eventType"] == "Microsoft.Storage.BlobCreated", ev
        assert ev["topic"] == out["account_id"], ev["topic"]
        assert ev["subject"].endswith("/containers/inbox/blobs/from-terraform.txt"), ev["subject"]


def check_template(emu: Emulator, out: dict):
    from azure.storage.blob import BlobServiceClient

    assert out["blob_endpoint"] == emu.url("blob", "tftemplate01") + "/", out["blob_endpoint"]
    ctx = ssl.create_default_context(cafile=str(emu.ca_file))
    req = urllib.request.Request(
        f"https://management.localhost:{emu.port}/subscriptions/{SUB}/resourceGroups/tf-template/providers/Microsoft.Storage/storageAccounts/tftemplate01/listKeys?api-version=2023-05-01",
        data=b"{}",
        method="POST",
        headers={"Authorization": "Bearer x", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, context=ctx, timeout=20) as r:
        key = json.load(r)["keys"][0]["value"]
    blobs = BlobServiceClient(out["blob_endpoint"], credential=key)
    blobs.get_container_client("templated").upload_blob("a.txt", b"from the template")
    assert blobs.get_blob_client("templated", "a.txt").download_blob().readall() == b"from the template"


CHECKS = {
    "base": check_base,
    "servicebus": check_servicebus,
    "eventhubs": check_eventhubs,
    "keyvault": check_keyvault,
    "appconfig": check_appconfig,
    "cosmos": check_cosmos,
    "eventgrid": check_eventgrid,
    "template": check_template,
}


def run_case(name: str, binary: pathlib.Path, cache: pathlib.Path, keep: bool, only_steps) -> bool:
    case_src = CASES_DIR / name
    work = pathlib.Path(tempfile.mkdtemp(prefix=f"azemu-tf-{name}-"))
    ok = True
    emu = None
    tf = None
    started_destroy = False
    results = []

    def step(label: str, fn):
        nonlocal ok
        t0 = time.monotonic()
        try:
            detail = fn()
            results.append((label, True))
            print(f"  {name:<12} {label:<8} ok    {time.monotonic() - t0:6.1f}s" + (f"  {detail}" if detail else ""), flush=True)
            return True
        except Exception as e:  # noqa: BLE001 - every failure is a result line
            ok = False
            results.append((label, False))
            msg = str(e).strip().splitlines()
            if not msg:  # a bare assert: say which line
                import traceback

                frame = traceback.extract_tb(e.__traceback__)[-1]
                msg = [f"{type(e).__name__} at {pathlib.Path(frame.filename).name}:{frame.lineno}: {frame.line}"]
            print(f"  {name:<12} {label:<8} FAIL  {time.monotonic() - t0:6.1f}s  {msg[0] if msg else type(e).__name__}", flush=True)
            for line in msg[1:12]:
                print("      " + line)
            return False

    try:
        shutil.copy(HERE / "provider.tf", work / "provider.tf")
        # the lock file pins the provider, so `init` finds it in the cache without asking the registry
        shutil.copy(HERE / ".terraform.lock.hcl", work / ".terraform.lock.hcl")
        for f in case_src.glob("*"):
            shutil.copy(f, work / f.name)
        emu = Emulator(binary, work)
        bundle = build_bundle(emu, work)
        tf = Terraform(emu, work, bundle, cache)
        var = ["-var", f"port={emu.port}"]

        def tf_step(label, args, *, trust, ok_codes=(0,), note=None):
            def go():
                code = tf.run(label, args, trust_emulator=trust)
                if code not in ok_codes:
                    raise RuntimeError(f"terraform {args[0]} exited {code}\n{tail(tf.last_output)}")
                return note(tf.last_output) if note else ""

            return go

        def counts(text):
            import re

            m = re.search(r"(Apply complete! Resources: .*?\.)|(Destroy complete! Resources: .*?\.)|(No changes\.)", text)
            return m.group(0) if m else ""

        def init():
            # a network hiccup while the provider is downloaded is not a failure of the emulator: try three times
            last = ""
            for _ in range(3):
                code = tf.run("init", ["init", "-input=false", "-no-color"], trust_emulator=False)
                if code == 0:
                    return ""
                last = tail(tf.last_output)
                time.sleep(3)
            raise RuntimeError(f"terraform init failed three times\n{last}")

        steps_ok = step("init", init)
        if steps_ok:
            steps_ok = step("apply", tf_step("apply", ["apply", "-auto-approve", "-input=false", "-no-color", *var], trust=True, note=counts))
            started_destroy = True
        if steps_ok:
            step("plan", tf_step("plan", ["plan", "-detailed-exitcode", "-input=false", "-no-color", *var], trust=True, note=counts))

            def checks():
                out = tf.outputs()
                fn = CHECKS.get(name)
                if fn:
                    fn(emu, out)
                return ""

            step("check", checks)
        if started_destroy:
            step("destroy", tf_step("destroy", ["destroy", "-auto-approve", "-input=false", "-no-color", *var], trust=True, note=counts))
            if emu is not None:
                step("gone", lambda: check_gone(emu))
    except Exception as e:  # noqa: BLE001
        ok = False
        print(f"  {name:<12} setup    FAIL  {e}", flush=True)
    finally:
        if emu is not None:
            emu.stop()
        if keep:
            print(f"  {name:<12} kept     {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)
    return ok


def check_gone(emu: Emulator):
    """After destroy the emulator holds no resource group of ours."""
    ctx = ssl.create_default_context(cafile=str(emu.ca_file))
    req = urllib.request.Request(
        f"https://management.localhost:{emu.port}/subscriptions/{SUB}/resourcegroups?api-version=2024-01-01",
        headers={"Authorization": "Bearer x"},
    )
    # `management.localhost` resolves to the loopback address on a Linux desktop, as `*.localhost` does for the SDK tests.
    with urllib.request.urlopen(req, context=ctx, timeout=20) as r:
        groups = [g["name"] for g in json.load(r)["value"]]
    if groups:
        raise AssertionError(f"resource groups left after destroy: {groups}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cases", nargs="*", help="case names (default: all)")
    ap.add_argument("--bin", default=str(ROOT / "target" / "debug" / "azemu"))
    ap.add_argument("--cache", default=os.environ.get("TF_PLUGIN_CACHE_DIR") or str(ROOT / "refs" / "terraform-plugin-cache"))
    ap.add_argument("--keep", action="store_true", help="keep the work directory of each case")
    ap.add_argument("--list", action="store_true", help="list the cases and stop")
    args = ap.parse_args()

    available = sorted(p.name for p in CASES_DIR.iterdir() if (p / "main.tf").exists())
    if args.list:
        print("\n".join(available))
        return 0
    wanted = args.cases or available
    unknown = [c for c in wanted if c not in available]
    if unknown:
        print(f"unknown case(s): {', '.join(unknown)}; available: {', '.join(available)}", file=sys.stderr)
        return 2
    binary = pathlib.Path(args.bin)
    if not binary.exists():
        print(f"{binary} does not exist; run `cargo build -p azemu` first", file=sys.stderr)
        return 2
    if not shutil.which("terraform"):
        print("terraform is not installed", file=sys.stderr)
        return 2
    cache = pathlib.Path(args.cache)
    cache.mkdir(parents=True, exist_ok=True)

    started = time.monotonic()
    results = {}
    for case in wanted:
        results[case] = run_case(case, binary, cache, args.keep, None)
    failed = [c for c, v in results.items() if not v]
    print(f"{len(results) - len(failed)}/{len(results)} cases passed in {time.monotonic() - started:.0f}s" + (f"; failed: {', '.join(failed)}" if failed else ""))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
