#!/usr/bin/env python3
"""Drives the Azure Functions host (func start) against a private copy of azemu and prints one line per function.

    tests/functions/run.sh                  everything
    tests/functions/run.sh --only timer,queue     only these groups (see GROUPS)
    tests/functions/run.sh --lenient        start azemu with AZEMU_LENIENT=1 and do not create queues, hubs, topics first
    tests/functions/run.sh --verbose        host in --verbose mode (default already writes the full host log to logs/)
    tests/functions/run.sh --keep           leave the scratch directory (logs are always copied to tests/functions/logs/)

See README.md. Needs: the .NET SDK (DOTNET_ROOT), Azure Functions Core Tools (`func` on the PATH, or FUNC_BIN), Python with the Azure SDKs.
"""
import sys; sys.path.insert(0, __import__("os").path.dirname(__import__("os").path.abspath(__file__)))
import argparse, base64, json, os, shutil, signal, socket, subprocess, sys, time, urllib.request, uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SCRATCH = Path(os.environ.get("AZEMU_FUNC_SCRATCH") or (Path.home() / ".cache" / "azemu-functions"))
DOTNET_ROOT = Path(os.environ["DOTNET_ROOT"]) if os.environ.get("DOTNET_ROOT") else ROOT / "refs/tools/dotnet"
FUNC = Path(os.environ.get("FUNC_BIN") or shutil.which("func") or ROOT / "refs/tools/func/node_modules/azure-functions-core-tools/bin/func")
OUT = HERE / "out"        # build output of FuncApp (replaced by NodeApp with --app node)
LOGS = HERE / "logs"

ACCOUNT = "funcacct"
KEY = base64.b64encode(b"funcs-test-key-0123456789abcdef").decode()
SB_NS, EH_NS, COSMOS_ACCT = "funcsb", "funceh", "funcdb"

procs = []          # (name, Popen) started by this script, stopped at the end
results = []        # (name, trigger, ok, detail)


def say(*a):
    print(*a, flush=True)


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def record(name, what, ok, detail=""):
    results.append((name, what, ok, detail))
    say(f"  [{'works' if ok else 'FAILS'}] {name}: {detail}")


def wait_for(fn, timeout=60, interval=1.0):
    """Call fn() until it returns something truthy; returns that value or None."""
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            last = fn()
            if last:
                return last
        except Exception as e:  # noqa: BLE001
            last = None
        time.sleep(interval)
    return None


def stop_all():
    for name, p in reversed(procs):
        if p.poll() is None:
            say(f"stopping {name} (pid {p.pid})")
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                p.wait(timeout=20)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(p.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                p.wait()


class Env:
    pass


def emulator_binary():
    """AZEMU_BIN, or the newer of target/release/azemu and target/debug/azemu."""
    if os.environ.get("AZEMU_BIN"):
        return Path(os.environ["AZEMU_BIN"])
    built = [p for p in (ROOT / "target/release/azemu", ROOT / "target/debug/azemu") if p.exists()]
    if not built:
        sys.exit("no emulator binary: build one (`cargo build --release -p azemu`) or set AZEMU_BIN")
    return max(built, key=lambda p: p.stat().st_mtime)


def start_emulator(env, lenient):
    work = env.work
    certs = work / "certs"; certs.mkdir(parents=True, exist_ok=True)
    home = work / "home"; home.mkdir(exist_ok=True)
    binary = SCRATCH / "azemu"
    src = emulator_binary()
    if not binary.exists() or binary.stat().st_mtime < src.stat().st_mtime:   # a rebuilt emulator is copied again
        shutil.copy2(src, binary)
    env.port = free_port()
    pins = []
    env.pin = {}
    if env.style == "path":
        for svc in ("blob", "queue", "table"):
            env.pin[svc] = free_port()
            pins += ["--pin", f"{svc}={env.pin[svc]}"]
    if env.identity and os.environ.get("STANDARD_PORTS"):
        pins += ["--standard-ports"]      # identity-based Service Bus dials 5671, which --standard-ports opens (README)
    elif env.identity:
        pins += ["--pin", "servicebus=5671"]
    for extra in os.environ.get("EXTRA_PINS", "").split(","):
        if extra:
            pins += ["--pin", extra]
    e = dict(os.environ, HOME=str(home), AZEMU_LENIENT="1" if lenient else "0")
    log = open(LOGS / "azemu.log", "w")
    p = subprocess.Popen([str(binary), "--port", str(env.port), "--cert-dir", str(certs), "--log-level", "debug"] + pins,
                         stdout=log, stderr=subprocess.STDOUT, env=e, start_new_session=True)
    procs.append(("azemu", p))
    ok = wait_for(lambda: urllib.request.urlopen(f"http://127.0.0.1:{env.port}/_azemu/health", timeout=2).status == 200, 30, 0.3)
    if not ok:
        raise SystemExit("azemu did not start; see logs/azemu.log")
    env.ca = certs / "ca.pem"
    # a bundle for tools that read one file: system roots + the emulator CA (host process only)
    bundle = work / "bundle.pem"
    sysca = Path("/etc/ssl/certs/ca-certificates.crt")
    bundle.write_bytes(sysca.read_bytes() + b"\n" + env.ca.read_bytes())
    env.bundle = bundle
    say(f"azemu pid {p.pid} on port {env.port}")


def host_of(env, kind, name):
    return f"{name}.{kind}.localhost:{env.port}"


TRIGGERS = {"eg": ["eg_trigger"], "timer": ["timer"], "queue": ["queue_to_blob", "queue_poison"], "blob": ["blob_poll"], "blobhost": ["blob_poll"], "blobeg": ["blob_eventgrid"],
            "sb": ["sb_queue", "sb_topic", "sb_poison"], "eh": ["eh_batch"], "cosmos": ["cosmos_changefeed"]}


def settings(env):
    p = env.port
    bp = getattr(env, "blob_proxy_port", p)   # --clamp-maxresults: the blob endpoint goes through blob_proxy.py
    storage = (f"DefaultEndpointsProtocol=http;AccountName={ACCOUNT};AccountKey={KEY};"
               f"BlobEndpoint=http://{ACCOUNT}.blob.core.localhost:{bp};"
               f"QueueEndpoint=http://{ACCOUNT}.queue.core.localhost:{p};"
               f"TableEndpoint=http://{ACCOUNT}.table.core.localhost:{p}")
    if env.style == "path":   # Azurite form: http://127.0.0.1:{port}/{account}; the host then scans containers (see README)
        storage = (f"DefaultEndpointsProtocol=http;AccountName={ACCOUNT};AccountKey={KEY};"
                   f"BlobEndpoint=http://127.0.0.1:{env.pin['blob']}/{ACCOUNT};"
                   f"QueueEndpoint=http://127.0.0.1:{env.pin['queue']}/{ACCOUNT};"
                   f"TableEndpoint=http://127.0.0.1:{env.pin['table']}/{ACCOUNT}")
    if getattr(env, "azurite", None):   # --azurite: Microsoft's Azurite instead of azemu, for comparison
        a = env.azurite
        storage = ("DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;"
                   f"BlobEndpoint=http://127.0.0.1:{a['blob']}/devstoreaccount1;QueueEndpoint=http://127.0.0.1:{a['queue']}/devstoreaccount1;"
                   f"TableEndpoint=http://127.0.0.1:{a['table']}/devstoreaccount1")
    s = {
        "FUNCTIONS_WORKER_RUNTIME": "node" if OUT.name == "NodeApp" else "dotnet-isolated",
        "AzureWebJobsStorage": storage,
        "ServiceBusConnection": (f"Endpoint=sb://{SB_NS}.servicebus.localhost:{p};SharedAccessKeyName=RootManageSharedAccessKey;"
                                 "SharedAccessKey=c2VjcmV0c2VjcmV0c2VjcmV0;UseDevelopmentEmulator=true"),
        "EventHubConnection": (f"Endpoint=sb://{EH_NS}.eventhub.localhost:{p};SharedAccessKeyName=RootManageSharedAccessKey;"
                               "SharedAccessKey=c2VjcmV0c2VjcmV0c2VjcmV0;UseDevelopmentEmulator=true"),
        "EgTopicUri": f"https://ftopic.eventgrid.localhost:{p}/api/events",
        "EgTopicKey": "azemu-key",
        "CosmosConnection": f"AccountEndpoint={'http' if os.environ.get('COSMOS_HTTP') else 'https'}://{COSMOS_ACCT}.documents.localhost:{p}/;AccountKey={KEY};",
        "FUNCTIONS_CORE_TOOLS_TELEMETRY_OPTOUT": "1",
        "DOTNET_CLI_TELEMETRY_OPTOUT": "1",
        "DOTNET_gcServer": "0",
        "DOTNET_GCHeapHardLimit": "0x30000000",
        "DOTNET_ROOT": str(DOTNET_ROOT),
        **({} if os.environ.get("NO_BUNDLE") else {"SSL_CERT_FILE": str(env.bundle)}),
        "AzureWebJobsFeatureFlags": "",
        "AzureFunctionsJobHost__logging__logLevel__default": os.environ.get("HOST_LOGLEVEL", "Information"),
    }
    for extra in os.environ.get("HOST_EXTRA", "").split(";"):    # experiments: HOST_EXTRA="k=v;k2=v2"
        if "=" in extra:
            k, v = extra.split("=", 1); s[k] = v.replace("{p}", str(p))
    # a trigger of a group that is not selected is disabled (one failing listener must not hide the others)
    on = {f for g, fns in TRIGGERS.items() if g in env.groups or ("restart" in env.groups and g in ("blob", "blobeg", "eh", "cosmos")) for f in fns}
    for fns in TRIGGERS.values():
        for f in fns:
            if f not in on:
                s[f"AzureWebJobs.{f}.Disabled"] = "true"
    return s


# ---- SDK clients for the checks ------------------------------------------------------------
def storage_clients(env):
    from azure.storage.blob import BlobServiceClient
    from azure.storage.queue import QueueServiceClient
    from azure.data.tables import TableServiceClient
    conn = settings(env)["AzureWebJobsStorage"]
    env.blob = BlobServiceClient.from_connection_string(conn)
    env.queue = QueueServiceClient.from_connection_string(conn)
    env.table = TableServiceClient.from_connection_string(conn)


def read_blob(env, container, name):
    try:
        return env.blob.get_blob_client(container, name).download_blob().readall().decode()
    except Exception:  # noqa: BLE001
        return None


def sb_admin(env):
    from azure.servicebus.management import ServiceBusAdministrationClient
    return ServiceBusAdministrationClient.from_connection_string(settings(env)["ServiceBusConnection"],
                                                                 connection_verify=str(env.ca))


def create_entities(env, lenient):
    say("creating entities")
    for c in ("markers", "inbox", "inbox-eg"):
        env.blob.create_container(c)
    if OUT.name == "NodeApp":
        env.queue.create_queue("nq-in")
        if not lenient and not os.environ.get("NO_CREATE"):
            sb_admin(env).create_queue("nsbq")
        return
    if os.environ.get("MAKE_LOGS"):
        env.blob.create_container("$logs")
    if getattr(env, "azurite", None):
        for q in ("q-in", "q-fail"):
            env.queue.create_queue(q)
        return
    if lenient or os.environ.get("NO_CREATE"):
        say("  (nothing but the blob containers is created first: queues, topics, hubs, Cosmos database and containers are left to the host)")
        return
    for q in ("q-in", "q-fail"):
        env.queue.create_queue(q)
    admin = sb_admin(env)
    admin.create_queue("sbq")
    admin.create_queue("sbq-fail", max_delivery_count=3)
    admin.create_topic("sbt")
    admin.create_subscription("sbt", "sub1", max_delivery_count=3)
    req = urllib.request.Request(f"http://127.0.0.1:{env.port}/_azemu/eventhubs/{EH_NS}/hubs/eh1", method="PUT",
                                 data=json.dumps({"partitionCount": 2}).encode(), headers={"content-type": "application/json"})
    urllib.request.urlopen(req).read()
    # Cosmos: database and containers (the lease container is created by the trigger)
    import ssl
    from azure.cosmos import CosmosClient
    os.environ["REQUESTS_CA_BUNDLE"] = str(env.bundle)
    cc = CosmosClient(f"https://{COSMOS_ACCT}.documents.localhost:{env.port}/", credential=KEY, connection_verify=str(env.bundle))
    db = cc.create_database_if_not_exists("appdb")
    db.create_container_if_not_exists("items", partition_key={"paths": ["/id"], "kind": "Hash"})
    env.cosmos_client = cc


def stop_proc(name):
    for n, p in list(procs):
        if n == name and p.poll() is None:
            os.killpg(p.pid, signal.SIGTERM)
            try:
                p.wait(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(p.pid, signal.SIGKILL); p.wait()


def host_settings(env):
    """What the host gets: the connection strings, or (--identity) the identity-based form with a managed identity endpoint."""
    s = settings(env)
    if not env.identity:
        return s
    p = env.port
    for k in ("AzureWebJobsStorage", "ServiceBusConnection", "EventHubConnection", "CosmosConnection"):
        del s[k]
    s.update({
        "AzureWebJobsStorage__blobServiceUri": f"https://{ACCOUNT}.blob.core.localhost:{p}",
        "AzureWebJobsStorage__queueServiceUri": f"https://{ACCOUNT}.queue.core.localhost:{p}",
        "AzureWebJobsStorage__tableServiceUri": f"https://{ACCOUNT}.table.core.localhost:{p}",
        # the Service Bus SDK takes no port in a fully qualified namespace and dials :5671 (see README): the emulator is pinned to it
        "ServiceBusConnection__fullyQualifiedNamespace": f"{SB_NS}.servicebus.localhost",
        # Event Hubs: no port either, but the extension has host.json "customEndpointAddress", set here through the environment
        "EventHubConnection__fullyQualifiedNamespace": f"{EH_NS}.eventhub.localhost",
        "AzureFunctionsJobHost__extensions__eventHubs__customEndpointAddress": f"sb://{EH_NS}.eventhub.localhost:{p}",
        "CosmosConnection__accountEndpoint": f"https://{COSMOS_ACCT}.documents.localhost:{p}/",
        "IDENTITY_ENDPOINT": f"http://127.0.0.1:{p}/_azemu/identity/msi/token",
        "IDENTITY_HEADER": "azemu",
    })
    for extra in os.environ.get("IDENTITY_EXTRA", "").split(";"):
        if "=" in extra:
            k, v = extra.split("=", 1); s[k] = v.replace("{p}", str(p))
    return s


def start_host(env, verbose, logname="host.log"):
    s = host_settings(env)
    e = dict(os.environ); e.update(s)
    e["PATH"] = f"{DOTNET_ROOT}:{e['PATH']}"
    env.hport = getattr(env, "hport", None) or free_port()
    log = open(LOGS / logname, "w")
    env.hostlog = LOGS / logname
    cmd = [str(FUNC), "start", "--port", str(env.hport)]
    if verbose:
        cmd.append("--verbose")
    p = subprocess.Popen(cmd, cwd=OUT, stdout=log, stderr=subprocess.STDOUT, env=e, start_new_session=True)
    procs.append(("func host", p))
    env.host_proc = p
    url = f"http://127.0.0.1:{env.hport}"
    env.url = url

    def up():
        if p.poll() is not None:
            raise SystemExit(f"host exited with {p.returncode}; see logs/host.log")
        return urllib.request.urlopen(url + ("/api/node-ping" if OUT.name == "NodeApp" else "/api/ping"), timeout=3).read() == b"pong"
    t0 = time.time()
    ok = wait_for(up, 120, 1.0)
    say(f"host pid {p.pid}: ping {'ok' if ok else 'NOT answering'} after {time.time() - t0:.0f}s")
    return bool(ok)


def count_in_log(env, text):
    try:
        return env.hostlog.read_text(errors="replace").count(text)
    except OSError:
        return -1


def http(env, path):
    return urllib.request.urlopen(env.url + path, timeout=30).read().decode()


# ---- the groups -----------------------------------------------------------------------------
def api(env, method, path, body=None):
    req = urllib.request.Request(f"http://127.0.0.1:{env.port}{path}", method=method,
                                 data=None if body is None else json.dumps(body).encode(), headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        t = r.read().decode()
        return json.loads(t) if t else {}


def g_http(env):
    ok = wait_for(lambda: http(env, "/api/ping") == "pong", 5)
    record("ping", "HTTP trigger", bool(ok), "host answers on /api/ping")


def g_timer(env):
    v = wait_for(lambda: read_blob(env, "markers", "timer.txt"), 60)
    ok = v is not None
    detail = v or "no marker blob in 60s"
    if ok:
        # the singleton lock of the timer is a blob lease in azure-webjobs-hosts; the marker is rewritten every 5 s
        first = v
        second = wait_for(lambda: (lambda x: x if x and x != first else None)(read_blob(env, "markers", "timer.txt")), 20)
        ok = second is not None
        detail = f"fired twice: {first[-17:]} then {(second or '?')[-17:]}" if ok else "fired once only"
    record("timer", "Timer trigger + Blob output (singleton lock = blob lease)", ok, detail)


def g_queue(env):
    http(env, "/api/http_queue_send?m=m1")
    v = wait_for(lambda: read_blob(env, "markers", "queue-m1.txt"), 60)
    record("queue_to_blob", "HTTP -> Queue output; Queue trigger -> Blob output", v is not None, v or "no marker blob in 60s")
    http(env, "/api/http_queue_fail_send")

    def poison():
        q = env.queue.get_queue_client("q-fail-poison")
        return [m.content for m in q.receive_messages(max_messages=5, visibility_timeout=1)] or None
    v = wait_for(poison, 90)
    n = count_in_log(env, "Executing 'Functions.queue_poison'")
    body = base64.b64decode(v[0]).decode() if v else None
    record("queue_poison", "Queue trigger that throws -> -poison queue", v is not None and n == 3 and body == "boom",
           f"function ran {n} times (maxDequeueCount 3), poison queue holds {body!r}" if v else "nothing in q-fail-poison in 90s")


def g_blob(env):
    time.sleep(float(os.environ.get("BLOB_DELAY", "0")))
    env.blob.get_blob_client("inbox", "a.txt").upload_blob(b"hello", overwrite=True)
    v = wait_for(lambda: read_blob(env, "markers", "blob-poll-a.txt.txt"), int(os.environ.get("BLOB_WAIT", "60")))
    record("blob_poll", f"Blob trigger (polling, {env.style}-style endpoints)", v is not None, v or "no marker blob in time")


def g_blob_host(env):
    """The polling blob trigger with host-name style storage endpoints (`http://{account}.blob.core.localhost:{port}`, the style the
    main README shows first). The host then cannot use the container scan alone: it enables Storage Analytics logging, lists `$logs`
    and reads the log blobs for new blobs. Before the logging was built this stopped the whole host."""
    if env.style != "host":
        say("  restarting the host with host-name style storage endpoints")
        stop_proc("func host")
        env.style = "host"
        if not start_host(env, False, "host-hostname.log"):
            record("blob_poll_host", "Blob trigger (polling), host-name endpoints", False, "host did not start; see logs/host-hostname.log"); return
    time.sleep(float(os.environ.get("BLOB_DELAY", "0")))
    env.blob.get_blob_client("inbox", "h.txt").upload_blob(b"hello-host", overwrite=True)
    v = wait_for(lambda: read_blob(env, "markers", "blob-poll-h.txt.txt"), int(os.environ.get("BLOB_WAIT", "90")))
    alive = env.host_proc.poll() is None
    log = env.hostlog.read_text(errors="replace")
    died = "Host is shutting down" in log
    # the logging the host asked for: $logs exists, holds a log blob with write operations, and the PutBlob of the trigger's input is in it
    logs = []
    try:
        logs = [b.name for b in env.blob.get_container_client("$logs").list_blobs(name_starts_with="blob/", include=["metadata"])
                if "write" in (b.metadata or {}).get("LogType", "")]
    except Exception as e:  # noqa: BLE001
        logs = [f"error {type(e).__name__}: {e}"]
    via = "LogsAndContainerScan" if "LogsAndContainerScan" in log else ("ContainerScan" if "ContainerScan" in log else "?")
    record("blob_poll_host", "Blob trigger (polling), host-name style endpoints, Storage Analytics logging",
           v == "blob 'h.txt' content 'hello-host'" and alive and not died and bool(logs) and not logs[0].startswith("error"),
           f"marker {v!r}, host {'up' if alive and not died else 'DOWN'}, write log blobs in $logs: {len(logs)}, trigger source in host log: {via}")


def g_blob_eg(env):
    key = json.loads(http(env, "/admin/host/systemkeys/blobs_extension"))["value"]
    hook = f"{env.url}/runtime/webhooks/blobs?functionName=Host.Functions.blob_eventgrid&code={key}"
    sub = api(env, "PUT", f"/_azemu/eventgrid/sources/storage/{ACCOUNT}/subscriptions/blobfn",
              {"webhookUrl": hook, "filter": {"subjectBeginsWith": "/blobServices/default/containers/inbox-eg/"}})
    say(f"  subscription validation: {json.dumps(sub.get('validation', sub))[:200]}")
    env.blob.get_blob_client("inbox-eg", "b.txt").upload_blob(b"hello-eg", overwrite=True)
    v = wait_for(lambda: read_blob(env, "markers", "blob-eg-b.txt.txt"), 40)
    if v is None:
        try:
            say(f"  subscription state: {json.dumps(api(env, 'GET', f'/_azemu/eventgrid/sources/storage/{ACCOUNT}/subscriptions/blobfn'))[:600]}")
        except Exception as e:  # noqa: BLE001
            say(f"  (could not read subscription: {e})")
    record("blob_eventgrid", "Blob trigger (Event Grid source) + storage events", v is not None, v or "no marker blob in 40s")


def g_table(env):
    http(env, "/api/http_table_put?pk=p1&rk=r1&v=hello-table")
    v = wait_for(lambda: http(env, "/api/http_table_get?pk=p1&rk=r1"), 20)
    row = env.table.get_table_client("fitems").get_entity("p1", "r1")
    record("table_in_out", "Table output and input binding", v == "hello-table" and row["Value"] == "hello-table",
           f"input binding answered {v!r}; SDK reads the row: {row['Value']!r}")


def g_sb(env):
    http(env, "/api/http_sb_send?m=hello-sb")
    v = wait_for(lambda: read_blob(env, "markers", "sb-queue.txt"), 60)
    left = None
    if v is not None and not env.lenient:
        time.sleep(2)
        left = sb_admin(env).get_queue_runtime_properties("sbq").active_message_count
    record("sb_queue", "HTTP -> Service Bus output; queue trigger (auto-complete) -> Blob output", v is not None and left in (None, 0),
           (v or "no marker blob in 60s") + ("" if left is None else f"; active messages left in the queue: {left}"))
    http(env, "/api/http_sbt_send?m=hello-topic")
    v = wait_for(lambda: read_blob(env, "markers", "sb-topic.txt"), 60)
    record("sb_topic", "HTTP -> Service Bus topic output; topic subscription trigger", v is not None, v or "no marker blob in 60s")
    http(env, "/api/http_sb_fail_send")
    from azure.servicebus import ServiceBusClient, ServiceBusSubQueue

    def dlq():
        with ServiceBusClient.from_connection_string(settings(env)["ServiceBusConnection"]) as c:
            with c.get_queue_receiver("sbq-fail", sub_queue=ServiceBusSubQueue.DEAD_LETTER, max_wait_time=3) as r:
                ms = r.receive_messages(max_message_count=5, max_wait_time=3)
                return [(str(m), m.dead_letter_reason) for m in ms] or None
    v = wait_for(dlq, 90, 2)
    n = count_in_log(env, "Executing 'Functions.sb_poison'")
    want = 10 if env.lenient else 3     # an auto-created queue has Azure's default max delivery count, 10
    record("sb_poison", "Service Bus trigger that throws -> dead-letter after MaxDeliveryCount", v is not None and n == want,
           f"function ran {n} times (max delivery count {want}); DLQ holds {v}" if v else "nothing in the DLQ in 90s")


def g_eh(env):
    http(env, "/api/http_eh_send?tag=e")
    v = wait_for(lambda: read_blob(env, "markers", "eh.txt"), 90)
    record("eh_batch", "HTTP -> Event Hubs output; Event Hubs trigger (batch) -> Blob output", v == "eh batch of 3: e1|e2|e3", v or "no marker blob in 90s")
    cps = wait_for(lambda: [b.name for b in env.blob.get_container_client("azure-webjobs-eventhub").list_blobs() if "/checkpoint/" in b.name] or None, 20)
    record("eh_checkpoint", "Event Hubs blob checkpoint store (azure-webjobs-eventhub)", cps is not None, f"checkpoint blobs: {len(cps or [])}")


def g_cosmos(env):
    http(env, "/api/http_cosmos_put?id=c1&v=hello-cosmos")
    v = wait_for(lambda: read_blob(env, "markers", "cosmos.txt"), 90)
    record("cosmos_changefeed", "HTTP -> Cosmos output; Cosmos DB trigger (change feed, lease container) -> Blob output", v == "cosmos change feed: 1 docs: c1=hello-cosmos", v or "no marker blob in 90s")
    v = wait_for(lambda: http(env, "/api/http_cosmos_get?id=c1"), 20)
    record("cosmos_input", "Cosmos DB input binding", v == "c1=hello-cosmos", f"answer {v!r}")


def g_eg(env):
    api(env, "PUT", "/_azemu/eventgrid/topics/ftopic", {})
    key = json.loads(http(env, "/admin/host/systemkeys/eventgrid_extension"))["value"]
    hook = f"{env.url}/runtime/webhooks/EventGrid?functionName=eg_trigger&code={key}"
    sub = api(env, "PUT", "/_azemu/eventgrid/topics/ftopic/subscriptions/fn", {"webhookUrl": hook})
    say(f"  subscription validation: {json.dumps(sub.get('validation', sub))[:200]}")
    http(env, "/api/http_eg_send")
    v = wait_for(lambda: read_blob(env, "markers", "eg-topic.txt"), 40)
    record("eventgrid_topic", "Event Grid output binding -> custom topic -> webhook -> Event Grid trigger", v is not None and "hello-eventgrid" in v, v or "no marker blob in 40s")


def g_restart(env):
    """Stop the host and start it again: receipts, checkpoints and leases must keep each trigger from running twice."""
    say("  restarting the host")
    for b in ("blob-poll-a.txt.txt", "blob-eg-b.txt.txt", "eh.txt", "cosmos.txt"):
        try:
            env.blob.get_blob_client("markers", b).delete_blob()
        except Exception:  # noqa: BLE001
            pass
    stop_proc("func host")
    if not start_host(env, False, "host-restart.log"):
        record("restart", "host restart", False, "host did not come back"); return
    http(env, "/api/http_eh_send?tag=f")
    http(env, "/api/http_cosmos_put?id=c2&v=second")
    v = wait_for(lambda: read_blob(env, "markers", "eh.txt"), 60)
    n_eh = count_in_log(env, "Executing 'Functions.eh_batch'")
    record("eh_restart", "Event Hubs trigger resumes from the blob checkpoint after a restart", v == "eh batch of 3: f1|f2|f3" and n_eh == 1,
           f"marker {v!r}, trigger ran {n_eh} time(s) after the restart")
    v = wait_for(lambda: read_blob(env, "markers", "cosmos.txt"), 60)
    n_c = count_in_log(env, "Executing 'Functions.cosmos_changefeed'")
    record("cosmos_restart", "Cosmos DB trigger resumes from its lease after a restart", v == "cosmos change feed: 1 docs: c2=second" and n_c == 1,
           f"marker {v!r}, trigger ran {n_c} time(s) after the restart")
    time.sleep(10)
    again = [b for b in ("blob-poll-a.txt.txt", "blob-eg-b.txt.txt") if read_blob(env, "markers", b) is not None]
    record("blob_receipts", "Blob receipts: already processed blobs do not run again after a restart", not again,
           "no blob trigger ran again" if not again else f"ran again: {again}")


def g_durable(env):
    r = json.loads(http(env, "/api/Start"))
    uri = {k.lower(): v for k, v in r.items()}["statusquerygeturi"]
    st = None
    def done():
        nonlocal st
        st = {k[0].lower() + k[1:]: v for k, v in json.loads(urllib.request.urlopen(uri, timeout=20).read().decode()).items()}
        return st.get("runtimeStatus") in ("Completed", "Failed", "Terminated") and st
    end = wait_for(done, 120, 2)
    ok = bool(end) and st.get("runtimeStatus") == "Completed" and st.get("output") == "Hello Tokyo!, Hello Seattle!, Hello London!"
    record("durable_orchestration", "Durable Functions: chaining, durable timer, fan-out/fan-in (queues, tables, blobs)", ok,
           f"{st.get('runtimeStatus')}: {st.get('output')!r}" if st else "no status")
    tables = [t.name for t in env.table.list_tables()]
    queues = [q.name for q in env.queue.list_queues()]
    containers = [c.name for c in env.blob.list_containers()]
    say(f"  tables {tables}\n  queues {queues}\n  containers {containers}")


def g_node(env):
    ok = wait_for(lambda: http(env, "/api/node-ping") == "pong", 10)
    record("node_ping", "Node worker: HTTP trigger", bool(ok), "answers" if ok else "no answer")
    from azure.storage.queue import QueueClient, TextBase64EncodePolicy
    # the host's queue trigger expects base64 text (host.json queues.messageEncoding, default Base64), so the sender must encode
    qc = QueueClient.from_connection_string(settings(env)["AzureWebJobsStorage"], "nq-in", message_encode_policy=TextBase64EncodePolicy())
    qc.send_message("nm1")
    v = wait_for(lambda: read_blob(env, "markers", "node-queue-nm1.txt"), 60)
    record("node_queue_to_blob", "Node worker: Queue trigger -> Blob output (extension bundle)", v is not None, v or "no marker blob in 60s")
    from azure.servicebus import ServiceBusClient, ServiceBusMessage
    with ServiceBusClient.from_connection_string(settings(env)["ServiceBusConnection"]) as c:
        with c.get_queue_sender("nsbq") as snd:
            snd.send_messages(ServiceBusMessage("nsb1"))
    v = wait_for(lambda: read_blob(env, "markers", "node-sb.txt"), 60)
    record("node_servicebus", "Node worker: Service Bus queue trigger -> Blob output (extension bundle)", v is not None, v or "no marker blob in 60s")


GROUPS = {"http": g_http, "timer": g_timer, "queue": g_queue, "blob": g_blob, "blobeg": g_blob_eg, "eg": g_eg, "table": g_table,
          "sb": g_sb, "eh": g_eh, "cosmos": g_cosmos, "restart": g_restart, "blobhost": g_blob_host}
NODE_GROUPS = {"http": g_node}
DURABLE_GROUPS = {"durable": g_durable}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default=",".join(GROUPS), help="comma separated groups: " + ",".join(GROUPS))
    ap.add_argument("--lenient", action="store_true")
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("--clamp-maxresults", action="store_true", help="diagnostic: blob endpoint through a proxy that clamps maxresults to 5000")
    ap.add_argument("--azurite", action="store_true", help="comparison: storage on Microsoft's Azurite (refs/tools/azurite) instead of azemu; storage groups only")
    ap.add_argument("--storage-style", choices=["path", "host"], default="path", help="AzureWebJobsStorage endpoints: path = http://127.0.0.1:{pinned port}/{account} (default), host = http://{account}.blob.core.localhost:{port}")
    ap.add_argument("--identity", action="store_true", help="identity-based connections (__serviceUri / __fullyQualifiedNamespace) with DefaultAzureCredential through IDENTITY_ENDPOINT; https endpoints on the gateway port")
    ap.add_argument("--app", choices=["dotnet", "node", "durable"], default="dotnet", help="dotnet = FuncApp (isolated worker, net10.0); node = NodeApp (second data point: queue and Service Bus triggers with Blob output); durable = DurableApp (Durable Functions)")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--no-build", action="store_true")
    a = ap.parse_args()
    global OUT
    if a.app == "node":
        OUT = HERE / "NodeApp"
        a.only = "http"
        GROUPS.clear(); GROUPS.update(NODE_GROUPS)
    if a.app == "durable":
        OUT = HERE / "out-durable"
        a.only = "durable"
        GROUPS.clear(); GROUPS.update(DURABLE_GROUPS)
    groups = a.only.split(",")
    for tool, what in ((FUNC, "Core Tools"), (DOTNET_ROOT / "dotnet", ".NET SDK")):
        if not tool.exists():
            raise SystemExit(f"missing {what} at {tool}: see README.md")
    if not OUT.exists():
        raise SystemExit(f"{OUT} is missing: run tests/functions/run.sh (it builds) or see README.md")
    LOGS.mkdir(exist_ok=True)
    env = Env()
    env.groups = groups
    env.lenient = a.lenient
    env.identity = a.identity
    if a.identity:
        a.storage_style = "host"
    env.style = a.storage_style
    env.work = SCRATCH / f"run-{os.getpid()}"
    env.work.mkdir(parents=True, exist_ok=True)
    try:
        start_emulator(env, a.lenient)
        if a.azurite:
            env.azurite = {k: free_port() for k in ("blob", "queue", "table")}
            ad = env.work / "azurite"; ad.mkdir()
            ap_ = subprocess.Popen(["node", str(ROOT / "refs/tools/azurite/node_modules/.bin/azurite"), "--silent", "--skipApiVersionCheck",
                                    "--location", str(ad), "--blobPort", str(env.azurite["blob"]), "--queuePort", str(env.azurite["queue"]),
                                    "--tablePort", str(env.azurite["table"])], stdout=open(LOGS / "azurite.log", "w"), stderr=subprocess.STDOUT, start_new_session=True)
            procs.append(("azurite", ap_))
            time.sleep(3)
        if a.clamp_maxresults:
            import blob_proxy
            env.blob_proxy_port = free_port()
            blob_proxy.start(env.blob_proxy_port, env.port)
            say(f"blob proxy on {env.blob_proxy_port}")
        storage_clients(env)
        create_entities(env, a.lenient)
        if not start_host(env, a.verbose):
            record("host", "func start", False, "host did not answer /api/ping; see logs/host.log")
        else:
            for g in groups:
                say(f"group {g}")
                try:
                    GROUPS[g](env)
                except Exception as e:  # noqa: BLE001
                    record(g, "group", False, f"exception {type(e).__name__}: {e}")
    finally:
        try:
            u = urllib.request.urlopen(f"http://127.0.0.1:{env.port}/_azemu/unsupported", timeout=5).read().decode()
            (LOGS / "unsupported.json").write_text(u)
            say(f"GET /_azemu/unsupported: {u[:300]}")
        except Exception as e:  # noqa: BLE001
            say(f"(no /_azemu/unsupported: {e})")
        stop_all()
        try:
            urllib.request.urlopen  # noqa
        except Exception:
            pass
        if not a.keep:
            shutil.rmtree(env.work, ignore_errors=True)
    say("\nRESULT")
    for n, w, ok, d in results:
        say(f"{'works' if ok else 'FAILS':6} {n:20} {w}  --  {d[:150]}")
    return 0 if all(r[2] for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
