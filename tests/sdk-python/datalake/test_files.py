"""DataLakeFileClient: create / properties / metadata / http headers / delete / exists / names / expiry."""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.filedatalake import ContentSettings, DataLakeFileClient

from dlutil import (
    DEFAULT_FILE_ACL,
    DEFAULT_FILE_PERMISSIONS,
    SUPERUSER,
    assert_error,
    assert_status,
    put_file,
    read_file,
    tree,
)


# -- create ------------------------------------------------------------------
def test_create_file_returns_etag_and_last_modified(fs):
    resp = fs.get_file_client("f.txt").create_file()
    assert resp["etag"]
    assert resp["last_modified"] is not None


def test_file_system_create_file_returns_client(fs):
    f = fs.create_file("dir1/dir2/file")
    assert isinstance(f, DataLakeFileClient)
    assert f.path_name == "dir1/dir2/file"
    assert f.exists()


def test_create_file_under_root_directory(fs):
    assert fs.get_file_client("filename").create_file() is not None
    assert tree(fs) == {"filename": False}


def test_create_file_creates_missing_parent_directories(fs):
    fs.create_file("a/b/c.txt")
    assert tree(fs) == {"a": True, "a/b": True, "a/b/c.txt": False}


def test_create_file_with_extra_slashes_in_client_arguments(emu, fs, service):
    """Recorded: filesystem "name/" and path "/dir/file" are normalised to /name/dir/file."""
    existing = fs.create_file("sub/file")
    from dlutil import DEV_KEY

    client = DataLakeFileClient(
        emu.url("dfs", "devstoreaccount1"),
        fs.file_system_name + "/",
        "/" + existing.path_name,
        credential=DEV_KEY,
    )
    assert client.create_file() is not None
    assert tree(fs) == {"sub": True, "sub/file": False}


def test_create_file_is_empty_and_has_default_properties(fs):
    f = fs.create_file("f")
    props = f.get_file_properties()
    assert props.size == 0
    assert props.name == "f"
    assert props.deleted is False
    assert props.content_settings.content_type == "application/octet-stream"
    assert props.lease.state == "available"
    assert props.lease.status == "unlocked"
    assert props.expiry_time is None


def test_create_file_overwrites_existing_and_truncates(fs):
    f = put_file(fs, "f", b"abc")
    assert f.get_file_properties().size == 3
    f.create_file()
    assert f.get_file_properties().size == 0
    assert read_file(f) == b""


def test_create_file_if_none_match_star_on_existing_raises_exists(fs):
    """Recorded: 409 PathAlreadyExists (JSON body on dfs)."""
    f = fs.create_file("f")
    with pytest.raises(ResourceExistsError) as e:
        f.create_file(match_condition=MatchConditions.IfMissing)
    assert_error(e, "PathAlreadyExists", 409)


def test_create_file_if_none_match_star_on_new_path_succeeds(fs):
    created = fs.get_file_client("new").create_file(match_condition=MatchConditions.IfMissing)
    assert created["etag"]


def test_create_file_with_content_settings_and_metadata(fs):
    f = fs.get_file_client("d/newfile")
    f.create_file(
        content_settings=ContentSettings(content_disposition="inline", content_language="spanish"),
        metadata={"hello": "world", "number": "42"},
    )
    props = f.get_file_properties()
    assert props.content_settings.content_disposition == "inline"
    assert props.content_settings.content_language == "spanish"
    assert props.metadata == {"hello": "world", "number": "42"}


def test_create_file_with_all_content_settings(fs):
    f = fs.get_file_client("f")
    f.create_file(
        content_settings=ContentSettings(
            content_type="text/plain",
            content_encoding="identity",
            content_language="en",
            content_disposition="attachment",
            cache_control="no-cache",
        )
    )
    cs = f.get_file_properties().content_settings
    assert cs.content_type == "text/plain"
    assert cs.content_encoding == "identity"
    assert cs.content_language == "en"
    assert cs.content_disposition == "attachment"
    assert cs.cache_control == "no-cache"


def test_create_file_with_lease_id_overwrites_leased_file(fs):
    """Recorded: acquire a lease via the blob endpoint, then create_file(lease=...) overwrites (201)."""
    f = fs.create_file("f")
    lease = f.acquire_lease(lease_id="00000000-1111-2222-3333-444444444444")
    f.create_file(lease=lease)
    props = f.get_file_properties()
    assert props.lease.state == "leased"
    assert props.lease.duration == "infinite"


def test_create_file_with_proposed_lease_and_duration(fs):
    f = fs.get_file_client("f")
    f.create_file(lease_id="4cf4e284-f6a8-4540-b53e-c3469af032dc", lease_duration=15)
    props = f.get_file_properties()
    assert props.lease.state == "leased"
    assert props.lease.duration == "fixed"


# -- properties --------------------------------------------------------------
def test_file_properties_after_write(fs):
    f = put_file(fs, "f", b"hello")
    props = f.get_file_properties()
    assert props.size == 5
    assert props.etag
    assert props.last_modified is not None
    assert props.creation_time is not None
    assert props.last_modified >= props.creation_time


def test_file_properties_owner_group_permissions_acl(fs):
    f = fs.create_file("f")
    props = f.get_file_properties(upn=True)
    assert props.owner == SUPERUSER
    assert props.group == SUPERUSER
    assert props.permissions == DEFAULT_FILE_PERMISSIONS
    assert props.acl == DEFAULT_FILE_ACL


def test_file_properties_etag_changes_with_content(fs):
    f = fs.create_file("f")
    e1 = f.get_file_properties().etag
    f.append_data(b"abc", 0, 3, flush=True)
    e2 = f.get_file_properties().etag
    assert e1 != e2


def test_file_properties_etag_stable_between_reads(fs):
    f = put_file(fs, "f", b"abc")
    assert f.get_file_properties().etag == f.get_file_properties().etag


def test_file_properties_missing_raises_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").get_file_properties()
    assert_error(e, "BlobNotFound", 404)


# -- http headers / metadata -------------------------------------------------
def test_set_http_headers(fs):
    f = fs.create_file("f")
    resp = f.set_http_headers(
        ContentSettings(content_type="application/json", content_language="fr", cache_control="max-age=60")
    )
    assert resp["etag"]
    assert resp["last_modified"] is not None
    cs = f.get_file_properties().content_settings
    assert cs.content_type == "application/json"
    assert cs.content_language == "fr"
    assert cs.cache_control == "max-age=60"


def test_set_http_headers_does_not_touch_data_or_metadata(fs):
    f = put_file(fs, "f", b"abc", metadata={"k": "v"})
    f.set_http_headers(ContentSettings(content_type="text/plain"))
    assert read_file(f) == b"abc"
    assert f.get_file_properties().metadata == {"k": "v"}


def test_set_metadata_round_trip(fs):
    f = fs.create_file("f")
    resp = f.set_metadata({"hello": "world", "number": "42"})
    assert resp["etag"]
    assert resp["last_modified"] is not None
    assert f.get_file_properties().metadata == {"hello": "world", "number": "42"}


def test_set_metadata_replaces_existing(fs):
    f = fs.create_file("f", metadata={"a": "1", "b": "2"})
    f.set_metadata({"c": "3"})
    assert f.get_file_properties().metadata == {"c": "3"}


def test_set_metadata_empty_clears(fs):
    f = fs.create_file("f", metadata={"a": "1"})
    f.set_metadata({})
    assert not f.get_file_properties().metadata


def test_metadata_values_with_spaces_and_punctuation(fs):
    f = fs.create_file("f")
    f.set_metadata({"key1": "hello world", "key2": "a=b;c,d"})
    md = f.get_file_properties().metadata
    assert md["key1"] == "hello world"
    assert md["key2"] == "a=b;c,d"


def test_file_metadata_is_not_inherited_by_implicit_parent_directory(fs):
    f = fs.create_file("d/f", metadata={"a": "1"})
    assert f.get_file_properties().metadata == {"a": "1"}
    assert fs.get_directory_client("d").get_directory_properties().metadata.get("a") is None


def test_set_metadata_on_missing_file_raises(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").set_metadata({"a": "1"})
    assert_status(e, 404)


# -- delete / exists ---------------------------------------------------------
def test_delete_file_then_properties_not_found(fs):
    f = fs.create_file("f")
    f.delete_file()
    with pytest.raises(ResourceNotFoundError):
        f.get_file_properties()


def test_file_system_delete_file(fs):
    fs.create_file("d/f")
    fs.delete_file("d/f")
    assert tree(fs) == {"d": True}


def test_delete_missing_file_raises_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").delete_file()
    assert_status(e, 404)


@pytest.mark.docs
def test_delete_missing_file_error_code(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").delete_file()
    assert_error(e, "PathNotFound", 404)


def test_delete_file_twice_second_fails(fs):
    f = fs.create_file("f")
    f.delete_file()
    with pytest.raises(ResourceNotFoundError):
        f.delete_file()


def test_delete_file_keeps_parent_directory(fs):
    fs.create_file("d/f")
    fs.delete_file("d/f")
    assert fs.get_directory_client("d").exists()


def test_file_exists_true_and_false(fs):
    f = fs.get_file_client("f")
    assert not f.exists()
    f.create_file()
    assert f.exists()
    f.delete_file()
    assert not f.exists()


def test_file_exists_in_missing_file_system_is_false(service, uniq):
    assert not service.get_file_client(uniq("nofs"), "f").exists()


def test_empty_file_downloads_as_empty_bytes(fs):
    """Recorded: the SDK's first ranged GET on an empty file gets 416 InvalidRange and retries unranged."""
    f = fs.create_file("empty")
    assert read_file(f) == b""
    assert f.get_file_properties().size == 0


# -- names -------------------------------------------------------------------
SPECIAL_NAMES = [
    "with space.txt",
    "café.txt",
    "データ.txt",
    "100%.txt",
    "%41%42.txt",
    "plus+sign.txt",
    "hash#tag.txt",
    "question?mark.txt",
    "amp&ersand.txt",
    "equals=sign.txt",
    "semi;colon.txt",
    "comma,comma.txt",
    "quote'single.txt",
    "paren(1).txt",
    "brackets[1].txt",
    "tilde~dash-under_score.txt",
    "at@sign.txt",
    "dollar$.txt",
    "?!@#$%^&*.?test",
]


@pytest.mark.parametrize("name", SPECIAL_NAMES, ids=[str(i) for i in range(len(SPECIAL_NAMES))])
def test_file_names_with_special_characters_round_trip(fs, name):
    f = put_file(fs, name, b"payload")
    assert f.exists()
    assert read_file(fs.get_file_client(name)) == b"payload"
    assert f.get_file_properties().name == name
    assert tree(fs) == {name: False}


def test_special_characters_in_directory_and_file_names(fs):
    path = "my dir/sub+dir/f#1?.txt"
    put_file(fs, path, b"x")
    assert tree(fs) == {"my dir": True, "my dir/sub+dir": True, path: False}
    assert read_file(fs.get_file_client(path)) == b"x"


def test_names_differing_only_by_percent_encoding_are_different_files(fs):
    put_file(fs, "a b", b"1")
    put_file(fs, "a%20b", b"2")
    assert read_file(fs.get_file_client("a b")) == b"1"
    assert read_file(fs.get_file_client("a%20b")) == b"2"


def test_deeply_nested_file(fs):
    path = "/".join(f"d{i}" for i in range(12)) + "/leaf.txt"
    put_file(fs, path, b"deep")
    assert read_file(fs.get_file_client(path)) == b"deep"
    assert len(tree(fs)) == 13


def test_long_file_name(fs):
    name = "n" * 200 + ".txt"
    put_file(fs, name, b"x")
    assert fs.get_file_client(name).exists()


def test_names_are_case_sensitive(fs):
    put_file(fs, "File.TXT", b"upper")
    put_file(fs, "file.txt", b"lower")
    assert read_file(fs.get_file_client("File.TXT")) == b"upper"
    assert read_file(fs.get_file_client("file.txt")) == b"lower"


# -- expiry ------------------------------------------------------------------
def test_create_file_with_relative_expiry(fs):
    f = fs.get_file_client("f")
    f.create_file(expires_on=86400000)  # one day, in milliseconds
    props = f.get_file_properties()
    assert props.expiry_time is not None
    delta = props.expiry_time - props.creation_time
    assert abs(delta - timedelta(days=1)) < timedelta(seconds=60)


def test_create_file_with_absolute_expiry(fs):
    f = fs.get_file_client("f")
    when = datetime(2075, 4, 4, tzinfo=timezone.utc)
    f.create_file(expires_on=when)
    assert f.get_file_properties().expiry_time == when


def test_set_file_expiry_absolute_then_never_expire(fs):
    f = fs.create_file("f")
    when = datetime.now(timezone.utc).replace(microsecond=0) + timedelta(hours=1)
    f.set_file_expiry("Absolute", expires_on=when)
    assert f.get_file_properties().expiry_time == when
    f.set_file_expiry("NeverExpire")
    assert f.get_file_properties().expiry_time is None


def test_set_file_expiry_relative_to_now(fs):
    f = fs.create_file("f")
    f.set_file_expiry("RelativeToNow", expires_on=3600 * 1000)
    props = f.get_file_properties()
    assert props.expiry_time is not None
    remaining = props.expiry_time - datetime.now(timezone.utc)
    assert timedelta(minutes=55) < remaining < timedelta(minutes=65)


def test_no_expiry_by_default_in_listing(fs):
    fs.create_file("f")
    [p] = list(fs.get_paths())
    assert p.expiry_time is None
