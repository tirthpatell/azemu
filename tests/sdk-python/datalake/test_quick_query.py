"""Quick query on Data Lake files: `DataLakeFileClient.query_file` (the SDK sends it to the blob endpoint of
the same account; `from DataLakeStorage` is rewritten to `from BlobStorage` by the SDK).

Expectations come from the SDK's own tests (recorded in refs/recordings, `test_quick_query.py` and
`test_cpk.py::test_query_file_cpk` of azure-storage-file-datalake) and the REST page "Query Blob Contents".
"""

import io

import pytest
import requests
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.filedatalake import DataLakeFileQueryError, DelimitedJsonDialect, DelimitedTextDialect

from dlutil import put_file

CSV = (
    "DataLakeStorage,Package,Version,RepoPath,MissingDocs\r\n"
    "App Configuration,azure-data-appconfiguration,1,appconfiguration,FALSE\r\n"
    "Event Hubs\r\n"
    "Identity,azure-identity,1.1.0-beta.1,identity,FALSE\r\n"
)
LINES = CSV.replace("\r\n", "\n")


@pytest.fixture
def csv_file(fs):
    return put_file(fs, "dir/data.csv", CSV.encode())


def test_select_star(csv_file):
    r = csv_file.query_file("SELECT * from DataLakeStorage")
    assert r.readall() == LINES.encode()
    assert len(r) == len(CSV)


def test_select_a_column_with_headers_in_and_out(csv_file):
    # the recorded request of test_quick_query_datalake_expression
    d = DelimitedTextDialect(delimiter=",", quotechar='"', lineterminator="\n", escapechar="", has_header=True)
    r = csv_file.query_file("SELECT DataLakeStorage from DataLakeStorage", file_format=d, output_format=d)
    assert r.readall() == b"DataLakeStorage\nApp Configuration\nEvent Hubs\nIdentity\n"


def test_where_and_readinto(csv_file):
    d = DelimitedTextDialect(has_header=True)
    out = io.BytesIO()
    csv_file.query_file(
        "SELECT Package, Version FROM BlobStorage WHERE RepoPath = 'identity'", file_format=d, output_format=DelimitedTextDialect()
    ).readinto(out)
    assert out.getvalue() == b"azure-identity,1.1.0-beta.1\n"


def test_json_file(fs):
    f = put_file(fs, "doc.json", b'{"name": "owner", "id": 1}\n{"name2": "owner2"}\n{"name": "mouse", "id": 7}')
    j = DelimitedJsonDialect()
    r = f.query_file("SELECT name from DataLakeStorage WHERE id >= 1", file_format=j, output_format=j)
    assert r.readall() == b'{"name":"owner"}\n{"name":"mouse"}\n'
    assert list(f.query_file("SELECT COUNT(*) FROM BlobStorage", file_format=j, output_format=DelimitedTextDialect()).records()) == [b"3", b""]


def test_errors_reach_the_handler(fs):
    f = put_file(fs, "bad.json", b"{name: owner}\n")
    errors = []
    r = f.query_file("SELECT * FROM BlobStorage", file_format=DelimitedJsonDialect(), on_error=errors.append)
    assert r.readall() == b""
    assert len(errors) == 1
    assert isinstance(errors[0], DataLakeFileQueryError)
    assert (errors[0].error, errors[0].is_fatal, errors[0].position) == ("ParseError", True, 0)


def test_a_missing_file(fs):
    with pytest.raises(ResourceNotFoundError):
        fs.get_file_client("nope.csv").query_file("SELECT * FROM BlobStorage")


def test_a_directory_has_no_rows(fs):
    fs.create_directory("d")
    assert fs.get_directory_client("d").create_file("x.csv") is not None
    # an empty file answers an empty result
    assert fs.get_file_client("d/x.csv").query_file("SELECT * FROM BlobStorage").readall() == b""


def test_the_same_data_through_the_dfs_endpoint(fs, emu, csv_file):
    """The dfs endpoint answers POST ?comp=query too (not used by the SDKs, which send it to the blob endpoint)."""
    from azure.storage.blob import generate_container_sas, ContainerSasPermissions
    from datetime import datetime, timedelta, timezone
    from dlutil import DEV_ACCOUNT, DEV_KEY

    sas = generate_container_sas(
        DEV_ACCOUNT, fs.file_system_name, DEV_KEY,
        permission=ContainerSasPermissions(read=True, list=True), expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )
    body = (
        "<QueryRequest><QueryType>SQL</QueryType><Expression>SELECT _1 FROM BlobStorage</Expression></QueryRequest>"
    )
    url = f"{emu.url('dfs', DEV_ACCOUNT)}/{fs.file_system_name}/dir/data.csv?comp=query&{sas}"
    r = requests.post(url, data=body, headers={"Content-Type": "application/xml"})
    assert r.status_code == 200, r.text
    assert r.headers["Content-Type"] == "avro/binary"
    assert r.content[:4] == b"Obj\x01"
    assert b"DataLakeStorage" in r.content and b"Event Hubs" in r.content
