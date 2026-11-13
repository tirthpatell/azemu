"""The driver: data directory options, timed saves, the lock, unreadable data, signals, the admin paths."""

import json
import os
import socket
import time
import urllib.error
import urllib.request

import pytest
from azure.storage.queue import QueueServiceClient

ACCOUNT = "devstoreaccount1"
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="


def queue(p):
    return QueueServiceClient(account_url=p.emu.url("queue", ACCOUNT), credential=KEY)


def post(p, path):
    req = urllib.request.Request(f"{p.emu.gateway}{path}", method="POST", data=b"")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def test_health_manifest_lock_and_save_answers(persistent):
    p = persistent()
    queue(p).create_queue("work").send_message("m")
    h = p.health()["persistence"]
    assert h["directory"] == str(p.data_dir) and h["saveIntervalSeconds"] == 15
    assert h["unsavedChanges"] is True and h["services"]["queue"]["unsavedChanges"] is True
    status, answer = post(p, "/_azemu/save")
    assert status == 200 and answer["services"]["queue"]["result"] == "saved"
    assert answer["services"]["blob"]["result"] in ("saved", "unchanged")
    assert "dfs" not in answer["services"], "Data Lake keeps nothing of its own: Blob owns the shared state"
    h = p.health()["persistence"]
    assert h["unsavedChanges"] is False
    assert h["services"]["queue"]["lastResult"] == "ok" and h["services"]["queue"]["lastSave"]
    status, again = post(p, "/_azemu/save")
    assert again["services"]["queue"]["result"] == "unchanged"
    manifest = json.loads((p.data_dir / "MANIFEST").read_text())
    assert manifest["layout"] == 1 and manifest["created_by"] and "queue" in manifest["services"]
    assert manifest["services"]["queue"]["files"]["queue.state"] == {"kind": "queue", "version": 1}
    assert (p.data_dir / "LOCK").read_text().strip() == str(p.proc.pid)
    p.stop()


def test_no_data_dir_changes_nothing(persistent):
    p = persistent(start=False)
    p.start(data_dir=False)
    assert "persistence" not in p.health()
    status, answer = post(p, "/_azemu/save")
    assert status == 409 and "NoDataDirectory" in json.dumps(answer)
    queue(p).create_queue("work")
    assert p.stop() == 0
    assert not p.data_dir.exists()


def test_timed_saves_survive_a_kill_and_interval_zero_saves_only_on_request(persistent):
    p = persistent(extra_args=["--save-interval", "1"])
    queue(p).create_queue("timed").send_message("kept by the timer")
    deadline = time.time() + 15
    while p.health()["persistence"]["unsavedChanges"] and time.time() < deadline:
        time.sleep(0.2)
    assert not p.health()["persistence"]["unsavedChanges"]
    p.stop(kill=True)
    p.start(extra_args=["--save-interval", "0"])
    assert [m.content for m in queue(p).get_queue_client("timed").peek_messages()] == ["kept by the timer"]
    queue(p).create_queue("manual").send_message("not saved")
    time.sleep(2.5)
    assert p.health()["persistence"]["unsavedChanges"] is True, "interval 0: no timed save"
    p.stop(kill=True)
    p.start()
    names = [q.name for q in queue(p).list_queues()]
    assert names == ["timed"]
    queue(p).create_queue("on-sigterm")
    assert p.stop() == 0
    p.start(extra_args=["--save-interval", "0"])
    assert sorted(q.name for q in queue(p).list_queues()) == ["on-sigterm", "timed"], "SIGTERM saved even with interval 0"
    p.stop()


def test_environment_variables_work_like_the_options(persistent):
    p = persistent(start=False)
    p.start(data_dir=False, env={"AZEMU_DATA_DIR": str(p.data_dir), "AZEMU_SAVE_INTERVAL": "7"})
    assert p.health()["persistence"]["saveIntervalSeconds"] == 7
    assert p.health()["persistence"]["directory"] == str(p.data_dir)
    queue(p).create_queue("fromenv")
    assert p.stop() == 0
    p.start(data_dir=False, env={"AZEMU_DATA_DIR": str(p.data_dir)})
    assert [q.name for q in queue(p).list_queues()] == ["fromenv"]
    p.stop()


def test_a_second_emulator_on_the_same_directory_is_refused(persistent):
    p = persistent()
    code, log = p.another().start(expect_failure=True)
    assert code != 0 and "in use by another running azemu" in log and str(p.proc.pid) in log, log
    # the first one is untouched
    queue(p).create_queue("still-works")
    p.stop()
    # a stale LOCK left by a process that died is taken over
    (p.data_dir / "LOCK").write_text("2147483000\n")
    p.start()
    p.stop()


def test_unreadable_state_stops_the_start_and_the_discard_flag_moves_it_aside(persistent):
    p = persistent()
    queue(p).create_queue("kept")
    from azure.keyvault.secrets import SecretClient  # noqa: F401

    p.save()
    p.stop()
    # damage the key vault state (which is in the directory, since the unified binary has every service)
    kv = p.data_dir / "keyvault"
    kv.mkdir(exist_ok=True)
    (kv / "keyvault.state").write_bytes(b"AZEMU-STATE keyvault 1\n\xc1\xc1")
    code, log = p.start(expect_failure=True)
    assert code != 0
    assert "keyvault" in log and "keyvault.state" in log and "--data-discard-unreadable" in log, log
    code, log = p.start(expect_failure=True)  # and again: nothing was changed by the refusal
    assert code != 0 and "keyvault.state" in log
    p.start(extra_args=["--data-discard-unreadable"])
    aside = [d.name for d in p.data_dir.iterdir() if d.name.startswith("keyvault.unreadable-")]
    assert len(aside) == 1 and (p.data_dir / aside[0] / "keyvault.state").exists()
    h = p.health()["persistence"]["services"]["keyvault"]
    assert "unreadableStateMovedTo" in h
    assert [q.name for q in queue(p).list_queues()] == ["kept"], "the other services are unaffected"
    p.stop()


def test_reset_with_save_empties_the_disk_too(persistent):
    p = persistent()
    queue(p).create_queue("gone-soon")
    p.save()
    status, answer = post(p, "/_azemu/reset?save=true")
    assert status == 200 and answer["services"]["queue"]["result"] == "saved"
    p.stop(kill=True)
    p.start()
    assert list(queue(p).list_queues()) == []
    # reset alone leaves the empty state to the next save
    queue(p).create_queue("again")
    p.save()
    assert post(p, "/_azemu/reset")[0] == 200
    assert p.health()["persistence"]["unsavedChanges"] is True
    p.stop()
    p.start()
    assert list(queue(p).list_queues()) == []
    p.stop()


def _hanging_request(p):
    """A request whose body never arrives completely: it is in progress until the server gives up on it."""
    s = socket.create_connection(("127.0.0.1", p.emu.port))
    host = f"{ACCOUNT}.queue.core.localhost:{p.emu.port}"
    s.sendall(f"POST /work/messages HTTP/1.1\r\nHost: {host}\r\nContent-Length: 100\r\n\r\npartial".encode())
    return s


def test_a_second_signal_quits_at_once_and_the_first_waits_for_requests_then_saves(persistent):
    p = persistent()
    queue(p).create_queue("before")
    p.save()
    queue(p).create_queue("unsaved")
    # a request that never finishes: the head is sent, the body is not
    s = _hanging_request(p)
    time.sleep(0.5)
    started = time.time()
    p.proc.send_signal(15)
    time.sleep(1.0)
    assert p.proc.poll() is None, "the first signal waits for the request in progress"
    p.proc.send_signal(15)
    assert p.proc.wait(timeout=3) != 0, "the second signal exits at once"
    assert time.time() - started < 4.5
    s.close()
    # no save happened on the quick exit, but the earlier state is not damaged
    p.start()
    assert sorted(q.name for q in queue(p).list_queues()) == ["before"]
    p.stop()
    # and without a second signal the wait is bounded and the save happens
    p.start()
    queue(p).create_queue("after")
    s = _hanging_request(p)
    time.sleep(0.5)
    t = time.time()
    assert p.stop(timeout=20) == 0
    waited = time.time() - t
    assert 3 < waited < 12, waited
    s.close()
    p.start()
    assert sorted(q.name for q in queue(p).list_queues()) == ["after", "before"]
    p.stop()
