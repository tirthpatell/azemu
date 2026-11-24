"""`azure-schemaregistry-avroencoder`: an event is encoded with a schema from the registry, goes through Event Hubs and is decoded with
the schema that its content type names."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.eventhub import EventData
from azure.schemaregistry.encoder.avroencoder import AvroEncoder

import srutil


def record(name, *extra):
    return srutil.order_schema(name, extra)


def test_encode_send_receive_decode(emu, client, group, hub, forget):
    name = "Order_" + srutil.uniq("t").split("-")[1]
    forget(f"azemu.sr.{name}")
    schema = record(name, ("qty", "int"))
    order = {"id": "o-1", "qty": 3}
    with AvroEncoder(client=client, group_name=group, auto_register=True) as encoder:
        event = encoder.encode(order, schema=schema, message_type=EventData)
        assert event.content_type.startswith("avro/binary+")
        schema_id = event.content_type.split("+", 1)[1]
        assert len(schema_id) == 32
        # the schema is in the registry under the full name of the record
        assert client.get_schema(schema_id).definition == schema

        marker, since = srutil.uniq("m"), srutil.start_position(emu)
        srutil.send(emu, hub, [srutil.tag(event, marker)])
        got = srutil.receive(emu, hub, 1, marker, since)
        assert got[0].content_type == event.content_type
        assert encoder.decode(got[0]) == order


def test_two_versions_of_a_schema_on_the_same_stream(emu, client, group, hub, forget):
    name = "Order_" + srutil.uniq("t").split("-")[1]
    forget(f"azemu.sr.{name}")
    v1, v2 = record(name), record(name, ("qty", "int"))
    with AvroEncoder(client=client, group_name=group, auto_register=True) as encoder:
        e1 = encoder.encode({"id": "a"}, schema=v1, message_type=EventData)
        e2 = encoder.encode({"id": "b", "qty": 2}, schema=v2, message_type=EventData)
        assert e1.content_type != e2.content_type
        marker, since = srutil.uniq("m"), srutil.start_position(emu)
        srutil.send(emu, hub, [srutil.tag(e1, marker), srutil.tag(e2, marker)])
        got = srutil.receive(emu, hub, 2, marker, since)
        # each event is decoded with the schema its own content type names (one partition: in the order they were sent)
        assert [encoder.decode(e) for e in got] == [{"id": "a"}, {"id": "b", "qty": 2}]


def test_without_auto_register_the_schema_has_to_be_there(client, group, forget):
    name = "Order_" + srutil.uniq("t").split("-")[1]
    forget(f"azemu.sr.{name}")
    schema = record(name)
    with AvroEncoder(client=client, group_name=group, auto_register=False) as encoder:
        with pytest.raises(Exception) as e:  # the encoder wraps what the registry answers
            encoder.encode({"id": "x"}, schema=schema)
        assert isinstance(e.value, (ResourceNotFoundError, HttpResponseError)) or isinstance(e.value.__cause__, (ResourceNotFoundError, HttpResponseError))
        client.register_schema(group, f"azemu.sr.{name}", schema, "Avro")
        content = encoder.encode({"id": "x"}, schema=schema)
        assert content["content_type"].startswith("avro/binary+")
        assert encoder.decode(content) == {"id": "x"}


def test_decoding_a_message_whose_schema_is_not_in_the_registry_fails(client, group):
    with AvroEncoder(client=client, group_name=group) as encoder:
        with pytest.raises(Exception) as e:
            encoder.decode({"content": b"\x00", "content_type": "avro/binary+258e22ae9c4d463abe61dbd251bdaab5"})
        assert isinstance(e.value, (ResourceNotFoundError, HttpResponseError)) or isinstance(e.value.__cause__, (ResourceNotFoundError, HttpResponseError))
