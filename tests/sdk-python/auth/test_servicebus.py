"""Service Bus with credentials of every kind (`docs/strict-auth.md`): shared access keys with the rights of their rule, shared access signatures that are
wrong in each way, Entra tokens with and without a role, and the administration API. Each test states one row of what Azure answered in November 2026; the
same tests run against the emulator (strict mode) and against Azure (`AZEMU_AZURE=1`, which creates nothing on the management plane and leaves the queues empty).

Where the refusal comes: a bad credential at `put-token` (the SDK: `ServiceBusAuthenticationError`), a missing claim at the link attach
(`ServiceBusAuthorizationError`); the administration API says 401 where 403 would be expected."""

import re
import time
from datetime import datetime, timedelta, timezone

import pytest
import requests
from azure.core.credentials import AzureNamedKeyCredential
from azure.core.exceptions import ClientAuthenticationError
from azure.servicebus import ServiceBusClient, ServiceBusMessage
from azure.servicebus.exceptions import ServiceBusAuthenticationError, ServiceBusAuthorizationError
from azure.servicebus.management import ServiceBusAdministrationClient

from authutil import FixedToken, SasCredential, sas_token

SCOPE = "https://servicebus.azure.net/.default"
KW = dict(retry_total=0, try_timeout=30)


# -- clients and helpers ---------------------------------------------------------------------------------------------------------------

def from_conn(cred, **kw):
    return ServiceBusClient.from_connection_string(cred.conn.rstrip(";"), **KW, **kw)


def with_credential(world, credential):
    """A client for the namespace with a credential object. The emulator speaks plain AMQP to its own port."""
    extra = {} if world.azure else {"use_tls": False}
    return ServiceBusClient(world.sb.host, credential, **KW, **extra)


def admin_for(world, credential):
    kw = {} if world.azure else {"connection_verify": world.emu.ca_file}
    return ServiceBusAdministrationClient(world.sb.host, credential, retry_total=0, **kw)


def uri(world, entity=""):
    return f"sb://{world.sb.host}/{entity}"


def send_one(client, queue, text="auth"):
    with client.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage(text))


def receive_some(client, queue):
    with client.get_queue_receiver(queue, max_wait_time=3) as r:
        return r.receive_messages(max_message_count=1, max_wait_time=3)


def drain(world, queue):
    """Take what is in the queue away (the main credential)."""
    with from_conn(world.sb.root) as c:
        with c.get_queue_receiver(queue, max_wait_time=3) as r:
            for m in r.receive_messages(max_message_count=20, max_wait_time=3):
                r.complete_message(m)


@pytest.fixture(autouse=True)
def _empty_queues(world):
    if world.sb is None:
        pytest.skip("no Service Bus namespace in this run")
    drain(world, world.sb.entity)
    yield
    drain(world, world.sb.entity)


def claim_text(e, claim):
    t = str(e.value)
    assert "amqp:unauthorized-access" in t, t
    assert f"'{claim}' claim(s) are required to perform this operation." in t, t
    assert re.search(r"Resource: 'sb://[^/']+/[^']*'", t), t
    return t


# -- shared access keys: the rights are checked at the link attach -----------------------------------------------------------------------

def test_the_root_key_sends_and_receives(world):
    with from_conn(world.sb.root) as c:
        send_one(c, world.sb.entity)
        (m,) = receive_some(c, world.sb.entity) or [None]
        assert m is not None


def test_a_send_only_rule_sends_and_cannot_receive_or_peek(world):
    with from_conn(world.sb.send) as c:
        send_one(c, world.sb.entity)
        with pytest.raises(ServiceBusAuthorizationError) as e:
            receive_some(c, world.sb.entity)
        t = claim_text(e, "Listen")
        assert f"/{world.sb.entity}'" in t
        with pytest.raises(ServiceBusAuthorizationError) as e:
            with c.get_queue_receiver(world.sb.entity, max_wait_time=3) as r:
                r.peek_messages(max_message_count=1)
        claim_text(e, "Listen")


def test_a_listen_only_rule_receives_and_cannot_send(world):
    with from_conn(world.sb.listen) as c:
        assert receive_some(c, world.sb.entity) == []
        with pytest.raises(ServiceBusAuthorizationError) as e:
            send_one(c, world.sb.entity)
        claim_text(e, "Send")
        with pytest.raises(ServiceBusAuthorizationError) as e:
            with c.get_queue_sender(world.sb.entity) as s:
                s.schedule_messages(ServiceBusMessage("later"), datetime.now(timezone.utc) + timedelta(hours=1))
        claim_text(e, "Send")


def test_a_queue_level_send_rule_works_on_its_queue_only(world):
    if world.sb.qsend is None:
        pytest.skip("no queue-level rule")
    with from_conn(world.sb.qsend) as c:
        send_one(c, world.sb.entity)
        with pytest.raises(ServiceBusAuthorizationError) as e:
            receive_some(c, world.sb.entity)
        claim_text(e, "Listen")
    # the rule's name and key as a credential for another queue: refused at put-token, as an invalid signature
    if world.sb.other:
        cred = AzureNamedKeyCredential(world.sb.qsend.name, world.sb.qsend.key)
        with with_credential(world, cred) as c:
            with pytest.raises(ServiceBusAuthenticationError):
                send_one(c, world.sb.other)


# -- shared access signatures that are wrong ----------------------------------------------------------------------------------------------

def sas_client(world, token):
    return with_credential(world, SasCredential(token))


def test_a_sas_token_of_the_root_rule_for_the_queue_or_the_namespace_is_accepted(world):
    r = world.sb.root
    for resource in (uri(world, world.sb.entity), uri(world), uri(world, world.sb.entity).replace("sb://", "http://")):
        with sas_client(world, sas_token(resource, r.name, r.key, int(time.time()) + 3600)) as c:
            send_one(c, world.sb.entity)
            assert receive_some(c, world.sb.entity)


def test_an_expired_sas_token_is_refused_but_one_late_by_less_than_five_minutes_is_not(world):
    r = world.sb.root
    resource = uri(world, world.sb.entity)
    # found on Azure: expired 290 s ago is accepted, 300 s refused
    with sas_client(world, sas_token(resource, r.name, r.key, int(time.time()) - 290)) as c:
        send_one(c, world.sb.entity)
    for ago in (600, 3600):
        with sas_client(world, sas_token(resource, r.name, r.key, int(time.time()) - ago)) as c:
            with pytest.raises(ServiceBusAuthenticationError):
                send_one(c, world.sb.entity)


def test_a_wrong_signature_an_unknown_key_name_and_another_entity_are_refused(world):
    r = world.sb.root
    resource = uri(world, world.sb.entity)
    soon = int(time.time()) + 3600
    bad = {
        "another key": sas_token(resource, r.name, r.key + "x", soon),
        "unknown key name": sas_token(resource, "nobody", r.key, soon),
        "the name of the send-only rule with the root key": sas_token(resource, world.sb.send.name, r.key, soon),
    }
    if world.sb.other:
        bad["another entity"] = sas_token(uri(world, world.sb.other), r.name, r.key, soon)
    for what, token in bad.items():
        with sas_client(world, token) as c:
            with pytest.raises(ServiceBusAuthenticationError):
                send_one(c, world.sb.entity)


# -- Entra tokens ----------------------------------------------------------------------------------------------------------------------------------

def test_a_principal_with_a_data_role_sends_and_receives(world, main):
    with with_credential(world, main) as c:
        send_one(c, world.sb.entity)
        assert receive_some(c, world.sb.entity)


def test_a_receiver_cannot_send_and_may_receive(world, reader):
    with with_credential(world, reader) as c:
        with pytest.raises(ServiceBusAuthorizationError) as e:
            send_one(c, world.sb.entity)
        claim_text(e, "Send")
        assert receive_some(c, world.sb.entity) == []


def test_a_principal_with_no_role_passes_put_token_and_is_refused_at_the_attach(world, norole):
    with with_credential(world, norole) as c:
        with pytest.raises(ServiceBusAuthorizationError) as e:
            send_one(c, world.sb.entity)
        claim_text(e, "Send")
        with pytest.raises(ServiceBusAuthorizationError) as e:
            receive_some(c, world.sb.entity)
        claim_text(e, "Listen")


def test_a_role_on_one_queue_is_a_role_on_that_queue(world, scoped):
    with with_credential(world, scoped) as c:
        send_one(c, world.sb.entity)
        with pytest.raises(ServiceBusAuthorizationError) as e:
            receive_some(c, world.sb.entity)
        claim_text(e, "Listen")
        if world.sb.other:
            with pytest.raises(ServiceBusAuthorizationError) as e:
                send_one(c, world.sb.other)
            assert f"/{world.sb.other}'" in str(e.value)


def test_a_token_for_another_audience_or_with_its_signature_changed_is_refused_at_put_token(world, main):
    good = main.token(SCOPE)
    wrong_audience = main.token("https://vault.azure.net/.default")
    changed = good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB")
    for token in (wrong_audience, changed, "not-a-token"):
        with with_credential(world, FixedToken(token)) as c:
            with pytest.raises(ServiceBusAuthenticationError):
                send_one(c, world.sb.entity)


# -- the administration API ----------------------------------------------------------------------------------------------------------------------

def get(world, path, auth=None, method="GET"):
    h = {"Authorization": auth} if auth else {}
    return requests.request(method, f"{world.sb.https}/{path}", headers=h, timeout=60, verify=True if world.azure else world.emu.ca_file)


def detail(r):
    m = re.search(r"<Detail>(.*?)</Detail>", r.text, re.S)
    return m.group(1) if m else ""


def stable(text):
    return re.sub(r"\s*TrackingId:.*", "", text, flags=re.S)


def sas_for(world, cred, entity="", ago=-3600):
    return sas_token(f"https://{world.sb.host}/{entity}", cred.name, cred.key, int(time.time()) - ago)


def bearer(principal):
    return f"Bearer {principal.token(SCOPE)}"


API = "api-version=2021-05"


def test_the_root_rule_manages_and_a_reader_reads(world, main, reader):
    q = world.sb.entity
    assert get(world, f"{q}?{API}", sas_for(world, world.sb.root, q)).status_code == 200
    assert get(world, f"$Resources/Queues?{API}", sas_for(world, world.sb.root)).status_code == 200
    assert get(world, f"$namespaceinfo?{API}", bearer(main)).status_code == 200
    # a receiver (and a Reader) may get and list, not make a queue
    assert get(world, f"{q}?{API}", bearer(reader)).status_code == 200
    assert get(world, f"$Resources/Queues?{API}", bearer(reader)).status_code == 200
    assert get(world, f"$namespaceinfo?{API}", bearer(reader)).status_code == 200
    r = get(world, f"azemu-auth-new?{API}", bearer(reader), method="PUT")
    assert r.status_code == 401
    assert r.headers["content-type"].startswith("application/xml")
    assert stable(detail(r)) == "Authorization failed for specified action: Manage,EntityWrite."
    r = get(world, f"azemu-auth-missing?{API}", bearer(reader), method="DELETE")
    assert (r.status_code, stable(detail(r))) == (404, "No service is hosted at the specified address.")


def test_a_sender_on_one_queue_gets_that_queue_and_nothing_else(world, scoped):
    q = world.sb.entity
    assert get(world, f"{q}?{API}", bearer(scoped)).status_code == 200
    for path in (f"$Resources/Queues?{API}", f"$namespaceinfo?{API}"):
        r = get(world, path, bearer(scoped))
        assert r.status_code == 401
        assert stable(detail(r)) == "Authorization failed for specified action: Manage,EntityRead,Send,Listen.", path
    r = get(world, f"azemu-auth-new?{API}", bearer(scoped), method="PUT")
    assert stable(detail(r)) == "Authorization failed for specified action: Manage,EntityWrite."
    r = get(world, f"azemu-auth-missing?{API}", bearer(scoped), method="DELETE")
    assert (r.status_code, stable(detail(r))) == (401, "Generic: .")


def test_no_role_is_401_with_the_actions_named(world, norole):
    q = world.sb.entity
    for path in (f"{q}?{API}", f"$Resources/Queues?{API}", f"$namespaceinfo?{API}"):
        r = get(world, path, bearer(norole))
        assert r.status_code == 401
        assert stable(detail(r)) == "Authorization failed for specified action: Manage,EntityRead,Send,Listen.", path
    r = get(world, f"azemu-auth-new?{API}", bearer(norole), method="PUT")
    assert stable(detail(r)) == "Authorization failed for specified action: Manage,EntityWrite."


def test_a_rule_without_manage_may_ask_for_the_namespace_information_and_nothing_else(world):
    q = world.sb.entity
    for cred in (world.sb.send, world.sb.listen):
        assert get(world, f"$namespaceinfo?{API}", sas_for(world, cred)).status_code == 200
        for path in (f"{q}?{API}", f"$Resources/Queues?{API}", f"$Resources/Topics?{API}"):
            r = get(world, path, sas_for(world, cred, q if q in path else ""))
            assert r.status_code == 401, path
            assert stable(detail(r)) == "Manage,EntityRead claims required for this operation.", path
        r = get(world, f"azemu-auth-new?{API}", sas_for(world, cred, "azemu-auth-new"), method="PUT")
        assert stable(detail(r)) == "Authorization failed for specified action: Manage,EntityWrite."
        r = get(world, f"azemu-auth-missing?{API}", sas_for(world, cred, "azemu-auth-missing"), method="DELETE")
        assert r.status_code == 404


def test_a_queue_level_rule_reads_nothing_through_the_administration_api(world):
    if world.sb.qsend is None:
        pytest.skip("no queue-level rule")
    q = world.sb.entity
    r = get(world, f"{q}?{API}", sas_for(world, world.sb.qsend, q))
    assert (r.status_code, stable(detail(r))) == (401, "Manage,EntityRead claims required for this operation.")
    for path, entity in ((f"$Resources/Queues?{API}", ""), (f"{world.sb.other}?{API}" if world.sb.other else None, world.sb.other)):
        if path is None:
            continue
        r = get(world, path, sas_for(world, world.sb.qsend, entity))
        assert (r.status_code, stable(detail(r))) == (401, "InvalidSignature: The token has an invalid signature."), path


def test_a_token_or_a_signature_that_is_wrong_says_which_way(world):
    q = world.sb.entity
    root = world.sb.root
    r = get(world, f"{q}?{API}", sas_for(world, root, q, ago=3600))
    assert r.status_code == 401
    assert re.match(r"ExpiredToken: The token is expired\. Expiration time: '[\d: Z-]+'\.", stable(detail(r))), detail(r)
    for token, text in (
        (sas_token(f"https://{world.sb.host}/{q}", root.name, root.key + "x", int(time.time()) + 3600), "InvalidSignature: The token has an invalid signature."),
        (sas_token(f"https://{world.sb.host}/{q}", "nobody", root.key, int(time.time()) + 3600), "InvalidSignature: The token has an invalid signature."),
        (sas_for(world, root, "somewhere-else"), "InvalidAudience: The authorization header contains a token with a wrong audience."),
    ):
        r = get(world, f"{q}?{API}", token)
        assert (r.status_code, stable(detail(r))) == (401, text)
    r = get(world, f"{q}?{API}", None)
    assert (r.status_code, stable(detail(r))) == (401, "claim is empty or token is invalid.")
    r = get(world, f"{q}?{API}", "SharedAccessSignature garbage")
    assert r.status_code == 500
    assert re.fullmatch(r"TrackingId:[^,]+", detail(r).strip()) or detail(r).startswith("TrackingId:"), detail(r)


def test_an_entra_token_that_is_wrong_says_which_way(world, main):
    q = world.sb.entity
    good = main.token(SCOPE)
    other_audience = main.token("https://vault.azure.net/.default")
    changed = good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB")
    for token, text in (
        (other_audience, "InvalidAudience: Invalid authorization token audience."),
        (changed, "InvalidSignature: Invalid authorization token signature."),
        ("garbage", "MalformedToken: ."),
    ):
        r = get(world, f"{q}?{API}", f"Bearer {token}")
        assert (r.status_code, stable(detail(r))) == (401, text)


def test_the_administration_client_sees_401_as_an_authentication_error(world, reader):
    with admin_for(world, reader) as a:
        assert a.get_queue(world.sb.entity).name == world.sb.entity
        with pytest.raises(ClientAuthenticationError) as e:
            a.create_queue("azemu-auth-new")
        assert e.value.status_code == 401
