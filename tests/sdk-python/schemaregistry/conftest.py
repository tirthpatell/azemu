"""Fixtures for the Schema Registry suite: the REST API of the Event Hubs namespace host, through `azure-schemaregistry` and the Avro encoder.

Expectations come from the REST specification (`Azure/azure-rest-api-specs`, schemaregistry data plane), the SDKs and a Standard namespace
(November 2026); `docs/services/schema-registry.md` lists what was compared. With AZEMU_AZURE=1 the same tests run against that namespace
(`srutil.py`, tests/sdk-python/README.md).
"""

import pytest

import srutil  # first: it puts ../eventhubs on the path


@pytest.fixture(scope="session")
def azemu_service():
    return "eventhubs"


@pytest.fixture
def group(emu):
    """The group the tests register in: Avro, Forward (what the namespace on Azure has)."""
    return srutil.ensure_group(emu)


@pytest.fixture
def names(emu, group):
    """`names()` gives a schema name that is deleted when the test ends (on both sides: the namespace on Azure holds few schemas)."""
    made = []

    def make(prefix: str = "azemu-sr") -> str:
        n = srutil.uniq(prefix)
        made.append(n)
        return n

    yield make
    for n in made:
        try:
            srutil.delete_schema(emu, group, n)
        except Exception:  # noqa: BLE001 - the namespace may be gone; the test has failed already
            pass


@pytest.fixture
def forget(emu, group):
    """`forget(name)` deletes a schema that a test made under a name it did not get from `names` (the Avro encoder names a schema by the
    full name of its record)."""
    made = []
    yield made.append
    for n in made:
        try:
            srutil.delete_schema(emu, group, n)
        except Exception:  # noqa: BLE001
            pass


@pytest.fixture
def client(emu, group):
    c = srutil.make_client(emu)
    yield c
    c.close()


@pytest.fixture
def hub(emu, request):
    """A one-partition event hub for this test."""
    return srutil.create_hub(emu, srutil.uniq("azemu-sr-hub"), request)
