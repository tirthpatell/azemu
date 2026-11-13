"""The async twins (azure.identity.aio)."""

import pytest
from azure.identity.aio import (
    ClientSecretCredential,
    DefaultAzureCredential,
    ManagedIdentityCredential,
    WorkloadIdentityCredential,
)

import idutil
from idutil import CLIENT_ID, SCOPES, TENANT, USER_ASSIGNED_ID, check_token

ALL_SCOPES = list(SCOPES)


@pytest.mark.parametrize("scope", ALL_SCOPES)
async def test_aio_managed_identity_app_service(emu, monkeypatch, scope):
    monkeypatch.setenv("IDENTITY_ENDPOINT", idutil.app_service_endpoint(emu))
    monkeypatch.setenv("IDENTITY_HEADER", "azemu")
    async with ManagedIdentityCredential() as cred:
        check_token(emu, await cred.get_token(scope), scope)
    async with ManagedIdentityCredential(client_id=USER_ASSIGNED_ID) as cred:
        check_token(emu, await cred.get_token(scope), scope, appid=USER_ASSIGNED_ID)


async def test_aio_default_azure_credential_app_service(emu, monkeypatch):
    monkeypatch.setenv("IDENTITY_ENDPOINT", idutil.app_service_endpoint(emu))
    monkeypatch.setenv("IDENTITY_HEADER", "azemu")
    scope = "https://vault.azure.net/.default"
    async with DefaultAzureCredential() as cred:
        check_token(emu, await cred.get_token(scope), scope)


@pytest.mark.parametrize("scope", ALL_SCOPES)
async def test_aio_managed_identity_imds(emu, monkeypatch, scope):
    monkeypatch.setenv("AZURE_POD_IDENTITY_AUTHORITY_HOST", idutil.imds_authority(emu))
    async with ManagedIdentityCredential() as cred:
        check_token(emu, await cred.get_token(scope), scope)


@pytest.mark.parametrize("scope", ALL_SCOPES)
async def test_aio_client_secret_credential(emu, scope):
    async with ClientSecretCredential(
        TENANT, CLIENT_ID, "any-secret", authority=idutil.host_authority(emu),
        disable_instance_discovery=True, connection_verify=emu.ca_file,
    ) as cred:
        check_token(emu, await cred.get_token(scope), scope, appid=CLIENT_ID, appidacr="1", tenant=TENANT)


async def test_aio_workload_identity(emu, tmp_path):
    f = tmp_path / "token"
    f.write_text("a.b.c")
    async with WorkloadIdentityCredential(
        tenant_id=TENANT, client_id=CLIENT_ID, token_file_path=str(f), authority=idutil.host_authority(emu),
        disable_instance_discovery=True, connection_verify=emu.ca_file,
    ) as cred:
        scope = "https://storage.azure.com/.default"
        check_token(emu, await cred.get_token(scope), scope, appid=CLIENT_ID, appidacr="2")
