"""Query Blob Contents (quick query) with the Python SDK: `BlobClient.query_blob`.

Expectations come from the Python SDK's own live tests (`test_quick_query.py`, as recorded in
refs/recordings), the REST page "Query Blob Contents" and the "Query acceleration SQL language
reference", not from the emulator's code.
"""

import json

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceModifiedError, ResourceNotFoundError
from azure.storage.blob import BlobQueryError, DelimitedJsonDialect, DelimitedTextDialect

# The blob of the SDK's tests: a header line and rows, with \r\n line ends, and a row with one column.
CSV = (
    "Service,Package,Version,RepoPath,MissingDocs\r\n"
    "App Configuration,azure-data-appconfiguration,1,appconfiguration,FALSE\r\n"
    "Event Hubs\r\n"
    "Identity,azure-identity,1.1.0-beta.1,identity,FALSE\r\n"
    "Key Vault - Keys,azure-security-keyvault-keys,4.2.0-beta.1,keyvault,FALSE\r\n"
)
LINES = CSV.replace("\r\n", "\n")


@pytest.fixture
def csv_blob(container, blob_name):
    c = container.get_blob_client(blob_name)
    c.upload_blob(CSV, overwrite=True)
    return c


def put(container, blob_name, data):
    c = container.get_blob_client(blob_name)
    c.upload_blob(data, overwrite=True)
    return c


def test_select_star_returns_the_blob_with_plain_line_ends(csv_blob):
    r = csv_blob.query_blob("SELECT * from BlobStorage")
    assert r.readall() == LINES.encode()
    assert len(r) == len(CSV)
    assert r.name == csv_blob.blob_name
    h = r.response_headers
    assert h["content_type"] == "avro/binary" or h.get("Content-Type") == "avro/binary", h


def test_readall_with_encoding(csv_blob):
    r = csv_blob.query_blob("SELECT * from BlobStorage", encoding="utf-8")
    assert r.readall() == LINES


def test_readinto_and_records(csv_blob):
    import io

    out = io.BytesIO()
    csv_blob.query_blob("SELECT * from BlobStorage").readinto(out)
    assert out.getvalue() == LINES.encode()
    recs = list(csv_blob.query_blob("SELECT * from BlobStorage").records())
    assert recs[0] == b"Service,Package,Version,RepoPath,MissingDocs"
    assert recs[-1] == b""  # the data ends with the record separator


def test_headers_names_and_where(csv_blob):
    inp = DelimitedTextDialect(has_header=True)
    out = DelimitedTextDialect()
    r = csv_blob.query_blob(
        "SELECT Service, Version FROM BlobStorage WHERE RepoPath = 'keyvault' OR repopath = 'identity'",
        blob_format=inp,
        output_format=out,
    )
    assert r.readall() == b"Identity,1.1.0-beta.1\nKey Vault - Keys,4.2.0-beta.1\n"


def test_output_headers_come_first(csv_blob):
    inp = DelimitedTextDialect(has_header=True)
    out = DelimitedTextDialect(has_header=True)
    r = csv_blob.query_blob("SELECT * from BlobStorage", blob_format=inp, output_format=out)
    assert r.readall() == LINES.encode()
    out = DelimitedTextDialect(has_header=False)
    r = csv_blob.query_blob("SELECT * from BlobStorage", blob_format=inp, output_format=out)
    assert r.readall() == LINES.split("\n", 1)[1].encode()


def test_custom_dialects(csv_blob):
    inp = DelimitedTextDialect(delimiter=",", quotechar='"', lineterminator="\n", escapechar="", has_header=False)
    out = DelimitedTextDialect(delimiter=";", quotechar="'", lineterminator=".", escapechar="\\", has_header=False)
    r = csv_blob.query_blob("SELECT * from BlobStorage", blob_format=inp, output_format=out)
    # a field that holds the record separator (`.` here) is put in quotes, as the service does it
    want = ".".join(";".join(f"'{f}'" if "." in f else f for f in line.split(",")) for line in LINES.split("\n")[:-1]) + "."
    assert r.readall() == want.encode()
    recs = list(csv_blob.query_blob("SELECT * from BlobStorage", blob_format=inp, output_format=out).records())
    assert recs[1] == b"App Configuration;azure-data-appconfiguration;1;appconfiguration;FALSE"


def test_ordinals_and_comparison_of_text_with_numbers(container, blob_name):
    b = put(container, blob_name, "100,200,300,400\n300,400,500,600\n")
    r = b.query_blob("SELECT _2 from BlobStorage WHERE _1 > 250")
    assert r.readall() == b"400\n"
    r = b.query_blob("SELECT _1, _4 + 1, CAST(_3 AS FLOAT) / 4 FROM BlobStorage")
    assert r.readall() == b"100,401,75\n300,601,125\n"


def test_aggregates(container, blob_name):
    b = put(container, blob_name, "1\n2\n3\n4\n")
    r = b.query_blob("SELECT COUNT(*), SUM(CAST(_1 AS INT)), AVG(CAST(_1 AS INT)), MIN(CAST(_1 AS INT)), MAX(CAST(_1 AS INT)) FROM BlobStorage")
    assert r.readall() == b"4,10,2.5,1,4\n"
    r = b.query_blob("SELECT COUNT(*) FROM BlobStorage WHERE _1 > 2")
    assert r.readall() == b"2\n"


def test_limit(container, blob_name):
    b = put(container, blob_name, "a\nb\nc\nd\n")
    assert b.query_blob("SELECT * FROM BlobStorage LIMIT 2").readall() == b"a\nb\n"
    assert b.query_blob("SELECT * FROM BlobStorage LIMIT 0").readall() == b""


def test_json_input_and_output(container, blob_name):
    data = '{"name": "owner", "id": 1}\n{"name2": "owner2"}\n{"name": "owner", "id": 1}'
    b = put(container, blob_name, data)
    inp = DelimitedJsonDialect(delimiter="\n")
    out = DelimitedJsonDialect(delimiter=";")
    r = b.query_blob("SELECT name from BlobStorage", blob_format=inp, output_format=out)
    assert r.readall() == b'{"name":"owner"};{};{"name":"owner"};'
    r = b.query_blob("SELECT * from BlobStorage WHERE id = 1", blob_format=inp, output_format=inp)
    assert r.readall() == b'{"name":"owner","id":1}\n{"name":"owner","id":1}\n'
    # only the input format given: the answer is JSON too
    r = b.query_blob("SELECT name from BlobStorage", blob_format=inp)
    assert r.readall() == b'{"name":"owner"}\n{}\n{"name":"owner"}\n'


def test_json_paths_and_missing(container, blob_name):
    doc = {"id": 1, "name": "mouse", "tags": ["wireless", "accessory"], "dimensions": {"length": 3},
           "warehouses": [{"latitude": 41.8, "longitude": -87.6}, {"latitude": 1.5}]}
    b = put(container, blob_name, json.dumps(doc))
    j = DelimitedJsonDialect()
    assert b.query_blob("SELECT latitude FROM BlobStorage[*].warehouses[*]", blob_format=j).readall() == b'{"latitude":41.8}\n{"latitude":1.5}\n'
    assert b.query_blob("SELECT length FROM BlobStorage[*].dimensions", blob_format=j).readall() == b'{"length":3}\n'
    got = b.query_blob("SELECT weight,warehouses[0].longitude,id,tags[1] FROM BlobStorage[*]", blob_format=j).readall()
    assert got == b'{"longitude":-87.6,"id":1,"_4":"accessory"}\n'
    got = b.query_blob("SELECT id FROM BlobStorage WHERE weight IS MISSING", blob_format=j).readall()
    assert got == b'{"id":1}\n'


def test_the_documented_functions(container, blob_name):
    b = put(container, blob_name, "x\n")
    q = lambda e: b.query_blob(f"SELECT {e} FROM BlobStorage").readall().decode().strip()
    assert q("CHARACTER_LENGTH('abcdefg')") == "7"
    assert q("CHAR_LENGTH(_1)") == "1"
    assert q("LOWER('AbCdEfG')") == "abcdefg"
    assert q("UPPER('AbCdEfG')") == "ABCDEFG"
    assert q("SUBSTRING('123456789', 1, 5)") == "23456"
    assert q("TRIM(BOTH '123' FROM '1112211Microsoft22211122')") == "Microsoft"
    assert q("DATE_ADD('minute', 1, CAST('2017-01-02T03:04:05.006Z' AS TIMESTAMP))") == "2017-01-02T03:05:05.006Z"
    assert q("DATE_DIFF('hour','2018-11-09T00:00+05:30','2018-11-09T01:00:23-08:00')") == "14"
    assert q("EXTRACT(YEAR FROM '2010-01-01T')") == "2010"
    assert q("TO_STRING(CAST('1969-07-20T20:18Z' AS TIMESTAMP),  'MMMM d, y')") == '"July 20, 1969"'
    assert q("TO_TIMESTAMP('2007T')") == "2007-01-01T00:00:00Z"
    assert q("UTCNOW()").endswith("Z")


def test_like_in_between_null(container, blob_name):
    b = put(container, blob_name, "apple,5,\nbanana,15,x\ncherry,25,\n")
    r = lambda w: b.query_blob(f"SELECT _1 FROM BlobStorage WHERE {w}").readall().decode().split()
    assert r("_1 LIKE '%an%'") == ["banana"]
    assert r("_1 NOT LIKE '%an%'") == ["apple", "cherry"]
    assert r("_1 IN ('apple', 'cherry')") == ["apple", "cherry"]
    assert r("_2 BETWEEN 10 AND 20") == ["banana"]
    assert r("_2 > 10 AND NOT _1 = 'cherry'") == ["banana"]


def test_quoted_fields_and_escapes(container, blob_name):
    b = put(container, blob_name, '"a,b","say ""hi""",c\n')
    assert b.query_blob("SELECT _2 FROM BlobStorage").readall() == b'"say ""hi"""\n'
    assert b.query_blob("SELECT _1, _3 FROM BlobStorage").readall() == b'"a,b",c\n'


def test_a_snapshot_can_be_queried_as_it_was(container, blob_name):
    b = put(container, blob_name, "old\n")
    snap = b.create_snapshot()
    b.upload_blob("new\n", overwrite=True)
    s = container.get_blob_client(blob_name, snapshot=snap["snapshot"])
    assert s.query_blob("SELECT * FROM BlobStorage").readall() == b"old\n"
    assert b.query_blob("SELECT * FROM BlobStorage").readall() == b"new\n"


def test_errors_arrive_as_records(container, blob_name):
    b = put(container, blob_name, "{name: owner}\n{name2: owner2}\n")
    errors = []
    r = b.query_blob(
        "SELECT * from BlobStorage",
        blob_format=DelimitedJsonDialect(),
        output_format=DelimitedTextDialect(delimiter=";", quotechar="'", lineterminator=".", escapechar="\\"),
        on_error=errors.append,
    )
    assert r.readall() == b""
    assert len(errors) == 1
    e = errors[0]
    assert isinstance(e, BlobQueryError)
    assert (e.error, e.is_fatal, e.position) == ("ParseError", True, 0)
    assert e.description == "Unexpected token 'n' at [byte: 1]. Expecting token '\"'. "


def test_a_row_that_misses_a_column_is_a_non_fatal_error(csv_blob):
    errors = []
    r = csv_blob.query_blob(
        "SELECT RepoPath from BlobStorage",
        blob_format=DelimitedTextDialect(has_header=True),
        output_format=DelimitedTextDialect(delimiter=";", quotechar="'", lineterminator=".", escapechar="\\"),
        on_error=errors.append,
    )
    assert r.readall() == b"appconfiguration.identity.keyvault."
    assert [(e.error, e.is_fatal) for e in errors] == [("InvalidColumnOrdinal", False)]
    assert errors[0].position == len("Service,Package,Version,RepoPath,MissingDocs\r\n") + len(
        "App Configuration,azure-data-appconfiguration,1,appconfiguration,FALSE\r\n"
    )
    assert errors[0].description == "Column ordinal 0 does not exist in row."


def test_a_bad_expression_is_a_fatal_error_record(csv_blob):
    errors = []
    r = csv_blob.query_blob("SELEKT * FROM BlobStorage", on_error=errors.append)
    assert r.readall() == b""
    assert [(e.error, e.is_fatal) for e in errors] == [("ParseError", True)]
    assert "SELECT" in errors[0].description


def test_conditions_and_missing_blobs(container, blob_name):
    b = put(container, blob_name, "a\n")
    props = b.get_blob_properties()
    with pytest.raises(ResourceModifiedError):
        b.query_blob("SELECT * FROM BlobStorage", etag="0x1", match_condition=MatchConditions.IfNotModified)
    assert b.query_blob("SELECT * FROM BlobStorage", etag=props.etag, match_condition=MatchConditions.IfNotModified).readall() == b"a\n"
    with pytest.raises(ResourceNotFoundError):
        container.get_blob_client("nope").query_blob("SELECT * FROM BlobStorage")


def test_the_lease_id_is_checked_when_given(container, blob_name):
    b = put(container, blob_name, "a\n")
    lease = b.acquire_lease()
    # reading a leased blob needs no lease id, a wrong one is refused
    assert b.query_blob("SELECT * FROM BlobStorage").readall() == b"a\n"
    assert b.query_blob("SELECT * FROM BlobStorage", lease=lease).readall() == b"a\n"
    with pytest.raises(HttpResponseError) as e:
        b.query_blob("SELECT * FROM BlobStorage", lease="00000000-0000-0000-0000-000000000000")
    assert e.value.status_code == 412


def test_only_block_blobs_can_be_queried(container, blob_name):
    c = container.get_blob_client(blob_name)
    c.create_append_blob()
    with pytest.raises(HttpResponseError) as e:
        c.query_blob("SELECT * FROM BlobStorage")
    assert e.value.status_code == 409
