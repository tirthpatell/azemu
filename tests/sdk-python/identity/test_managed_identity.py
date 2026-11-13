"""Managed identity through the two endpoints the libraries use on Azure:
App Service style (IDENTITY_ENDPOINT + IDENTITY_HEADER) and IMDS."""

import time

import pytest
from azure.core.exceptions import HttpResponseError
from azure.identity import DefaultAzureCredential, ManagedIdentityCredential
from azure.identity._credentials.imds import _get_request as imds_probe_request
from azure.identity._internal.managed_identity_client import ManagedIdentityClient

import idutil
from idutil import SCOPES, USER_ASSIGNED_ID, check_token

ALL_SCOPES = list(SCOPES)


@pytest.fixture
def app_service(emu, monkeypatch):
    monkeypatch.setenv("IDENTITY_ENDPOINT", idutil.app_service_endpoint(emu))
    monkeypatch.setenv("IDENTITY_HEADER", "azemu")


@pytest.fixture
def imds(emu, monkeypatch):
    monkeypatch.setenv("AZURE_POD_IDENTITY_AUTHORITY_HOST", idutil.imds_authority(emu))


# -- App Service style ---------------------------------------------------------------
@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_managed_identity_app_service(emu, app_service, scope):
    cred = ManagedIdentityCredential()
    tok = cred.get_token(scope)
    claims = check_token(emu, tok, scope, appidacr="2")
    assert tok.expires_on > time.time() + 3000
    # system assigned: the same identity every time
    assert check_token(emu, ManagedIdentityCredential().get_token(scope), scope)["oid"] == claims["oid"]


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_managed_identity_app_service_user_assigned_client_id(emu, app_service, scope):
    tok = ManagedIdentityCredential(client_id=USER_ASSIGNED_ID).get_token(scope)
    claims = check_token(emu, tok, scope, appid=USER_ASSIGNED_ID)
    system = check_token(emu, ManagedIdentityCredential().get_token(scope), scope)
    assert claims["oid"] != system["oid"]


def test_managed_identity_app_service_object_and_resource_id(emu, app_service):
    scope = "https://storage.azure.com/.default"
    obj = "cccccccc-0000-4000-8000-000000000003"
    tok = ManagedIdentityCredential(identity_config={"object_id": obj}).get_token(scope)
    assert check_token(emu, tok, scope)["oid"] == obj
    rid = "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/id1"
    tok = ManagedIdentityCredential(identity_config={"resource_id": rid}).get_token(scope)
    check_token(emu, tok, scope)


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_default_azure_credential_app_service(emu, app_service, scope):
    tok = DefaultAzureCredential().get_token(scope)
    check_token(emu, tok, scope)


def test_default_azure_credential_app_service_user_assigned(emu, app_service, monkeypatch):
    scope = "https://vault.azure.net/.default"
    tok = DefaultAzureCredential(managed_identity_client_id=USER_ASSIGNED_ID).get_token(scope)
    check_token(emu, tok, scope, appid=USER_ASSIGNED_ID)
    # AZURE_CLIENT_ID also selects the user-assigned identity for DefaultAzureCredential
    monkeypatch.setenv("AZURE_CLIENT_ID", USER_ASSIGNED_ID)
    tok = DefaultAzureCredential().get_token(scope)
    check_token(emu, tok, scope, appid=USER_ASSIGNED_ID)


def test_app_service_header_is_required(emu):
    """Without the header the emulator answers 400 as App Service does."""
    import urllib.error
    import urllib.request

    url = idutil.app_service_endpoint(emu) + "?resource=https://x&api-version=2019-08-01"
    with pytest.raises(urllib.error.HTTPError) as e:
        urllib.request.urlopen(url, timeout=10)
    assert e.value.code == 400


# -- IMDS ----------------------------------------------------------------------------
@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_managed_identity_imds(emu, imds, scope):
    tok = ManagedIdentityCredential().get_token(scope)
    check_token(emu, tok, scope)


def test_managed_identity_imds_user_assigned(emu, imds):
    scope = "https://storage.azure.com/.default"
    tok = ManagedIdentityCredential(client_id=USER_ASSIGNED_ID).get_token(scope)
    check_token(emu, tok, scope, appid=USER_ASSIGNED_ID)


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_default_azure_credential_imds(emu, imds, scope):
    tok = DefaultAzureCredential().get_token(scope)
    check_token(emu, tok, scope)


def test_imds_probe_is_answered_the_way_imds_does(emu, imds):
    """azure-identity probes IMDS with a token request that has no Metadata header and
    treats any HTTP answer (a 400 here) as "IMDS is there"."""
    client = ManagedIdentityClient(imds_probe_request)
    with pytest.raises(HttpResponseError) as e:
        client.request_token("https://storage.azure.com/.default", connection_timeout=1, retry_total=0)
    assert e.value.status_code == 400
    assert e.value.response.json()["error"] == "bad_request_102"
