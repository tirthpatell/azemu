"""The 401 challenge names the request's own domain, so no client needs a switch.

On Azure the challenge says `resource="https://vault.azure.net"` and the clients refuse it when the vault's host is not under
that name. The emulator's hosts are `<vault>.vault.localhost:<port>`: the challenge it sends names `vault.localhost:<port>` (the
host without its first label, with the port), which the clients accept. Where the host really is under `vault.azure.net` (a
hosts-file set-up) the real name is kept.
"""

import re

import pytest
import requests

from kvutil import FakeCredential, unique, vault_url

API = "api-version=7.5"


def _challenge(emu, vault="chvault"):
    r = requests.get(f"{vault_url(emu, vault)}/secrets/{unique('s')}?{API}", verify=emu.ca_file, timeout=10)
    assert r.status_code == 401
    return r.headers["WWW-Authenticate"]


def test_the_challenge_names_the_domain_of_the_request(emu):
    www = _challenge(emu)
    assert re.search(rf'resource="https://vault\.localhost:{emu.port}"', www), www


def test_the_clients_pass_their_own_check_without_a_switch(emu):
    """Secrets, keys and certificates clients, sync and async, built with the SDK's defaults (the check on)."""
    from azure.keyvault.certificates import CertificateClient
    from azure.keyvault.keys import KeyClient
    from azure.keyvault.secrets import SecretClient

    url = vault_url(emu, "nocheck")
    secrets = SecretClient(url, FakeCredential(), connection_verify=emu.ca_file)
    keys = KeyClient(url, FakeCredential(), connection_verify=emu.ca_file)
    certs = CertificateClient(url, FakeCredential(), connection_verify=emu.ca_file)
    secrets.set_secret("s", "v")
    assert secrets.get_secret("s").value == "v"
    assert keys.create_ec_key("k").name == "k"
    assert certs.get_certificate_policy  # the client exists; the call below goes through the same policy
    assert list(certs.list_properties_of_certificates()) == []


@pytest.mark.asyncio
async def test_the_async_clients_pass_their_own_check_without_a_switch(emu):
    from azure.keyvault.secrets.aio import SecretClient

    from kvutil import AsyncFakeCredential

    async with SecretClient(vault_url(emu, "nocheckaio"), AsyncFakeCredential(), connection_verify=emu.ca_file) as c:
        await c.set_secret("s", "v")
        assert (await c.get_secret("s")).value == "v"


def test_the_old_switch_still_works(emu):
    """`verify_challenge_resource=False` is still accepted and changes nothing for the emulator."""
    from azure.keyvault.secrets import SecretClient

    c = SecretClient(vault_url(emu, "switch"), FakeCredential(), verify_challenge_resource=False, connection_verify=emu.ca_file)
    c.set_secret("s", "v")
    assert c.get_secret("s").value == "v"
