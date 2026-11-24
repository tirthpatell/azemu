"""`azure-schemaregistry` (SchemaRegistryClient, sync and async) against the registry of the Event Hubs namespace host."""

import json
import re

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.schemaregistry import SchemaFormat, SchemaRegistryClient

import srutil


def schema(name, *extra):
    return srutil.order_schema(name.replace("-", "_"), extra)


def test_register_returns_the_properties_and_the_same_content_the_same_id(client, group, names):
    name = names()
    s1 = schema(name)
    p = client.register_schema(group, name, s1, "Avro")
    assert re.fullmatch(r"[0-9a-f]{32}", p.id), p.id
    assert (p.group_name, p.name, p.version, p.format) == (group, name, 1, SchemaFormat.AVRO)
    again = client.register_schema(group, name, s1, SchemaFormat.AVRO)
    assert (again.id, again.version) == (p.id, 1)
    # white space and the order of keys make no new schema
    spaced = json.dumps(json.loads(s1), indent=3)
    assert client.register_schema(group, name, spaced, "Avro").id == p.id


def test_changed_content_is_a_new_version_and_both_can_be_read(client, group, names):
    name = names()
    s1, s2 = schema(name), schema(name, ("qty", "int"))
    p1 = client.register_schema(group, name, s1, "Avro")
    p2 = client.register_schema(group, name, s2, "Avro")
    assert (p1.version, p2.version) == (1, 2) and p1.id != p2.id
    by_id = client.get_schema(p2.id)
    assert by_id.definition == s2
    assert (by_id.properties.id, by_id.properties.version, by_id.properties.name, by_id.properties.group_name) == (p2.id, 2, name, group)
    assert by_id.properties.format == SchemaFormat.AVRO
    v1 = client.get_schema(group_name=group, name=name, version=1)
    assert (v1.definition, v1.properties.id) == (s1, p1.id)
    assert client.get_schema(group_name=group, name=name, version=2).properties.id == p2.id


def test_get_schema_properties_finds_the_id_of_a_content(client, group, names):
    name = names()
    s1 = schema(name)
    p = client.register_schema(group, name, s1, "Avro")
    assert client.get_schema_properties(group, name, s1, "Avro").id == p.id
    with pytest.raises(ResourceNotFoundError) as e:
        client.get_schema_properties(group, name, schema(name, ("other", "string")), "Avro")
    assert "does not exist" in str(e.value)


def test_a_change_the_groups_mode_forbids_is_refused(client, group, names):
    name = names()
    client.register_schema(group, name, schema(name, ("qty", "int")), "Avro")
    # Forward: the old schema must read what the new one writes; `qty` has no default, so it cannot go
    with pytest.raises(HttpResponseError) as e:
        client.register_schema(group, name, schema(name), "Avro")
    assert e.value.status_code == 400
    assert "Avro schema validation failed: Forward compatibility test failed when checking against schema " in e.value.message
    assert f"{name}, version 1, id " in e.value.message


def test_a_schema_that_does_not_parse_is_refused(client, group, names):
    name = names()
    with pytest.raises(HttpResponseError) as e:
        client.register_schema(group, name, '{"type":"record","name":"X"}', "Avro")
    assert e.value.status_code == 400
    assert "Avro schema validation failed: 'fields' cannot be null for record at ''." in e.value.message


def test_a_schema_of_another_kind_than_the_group_is_refused(client, group, names):
    name = names()
    with pytest.raises(HttpResponseError) as e:
        client.register_schema(group, name, '{"type":"object"}', SchemaFormat.JSON)
    assert e.value.status_code == 415
    assert f"Schema of type Json cannot be created within group '{group}' that supports Avro schema type." in e.value.message


def test_unknown_group_schema_and_id(client, group, names):
    name = names()
    with pytest.raises(ResourceNotFoundError) as e:
        client.register_schema("azemu-no-such-group", name, schema(name), "Avro")
    assert "Schema group 'azemu-no-such-group' does not exist while auto-create is turned off." in e.value.message
    with pytest.raises(ResourceNotFoundError):
        client.get_schema("258e22ae9c4d463abe61dbd251bdaab5")
    with pytest.raises(ResourceNotFoundError):
        client.get_schema(group_name=group, name=name, version=1)
    with pytest.raises(HttpResponseError) as e:
        client.get_schema("not-an-id")
    assert e.value.status_code == 400


@pytest.mark.parametrize("api_version", ["2021-10", "2022-10"])
def test_the_api_versions_of_the_sdk(emu, group, names, api_version):
    name = names()
    with srutil.make_client(emu, api_version=api_version) as c:
        s = schema(name)
        p = c.register_schema(group, name, s, "Avro")
        assert c.get_schema(p.id).definition == s
        assert c.get_schema(group_name=group, name=name, version=1).properties.id == p.id


async def test_the_async_client(emu, group, names):
    from azure.schemaregistry.aio import SchemaRegistryClient as AioClient

    name = names()
    s1, s2 = schema(name), schema(name, ("qty", "int"))
    cred = srutil.credential(emu, aio=True)
    async with cred, AioClient(fully_qualified_namespace=srutil.fqdn(emu), credential=cred, **srutil.client_kwargs(emu)) as c:
        p1 = await c.register_schema(group, name, s1, "Avro")
        p2 = await c.register_schema(group, name, s2, "Avro")
        assert (p1.version, p2.version) == (1, 2)
        assert (await c.get_schema(p1.id)).definition == s1
        assert (await c.get_schema_properties(group, name, s2, "Avro")).id == p2.id
        with pytest.raises(ResourceNotFoundError):
            await c.get_schema("258e22ae9c4d463abe61dbd251bdaab5")
