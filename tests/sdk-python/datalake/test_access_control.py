"""POSIX-style access control: owner / group / permissions / ACL / umask / recursive ACL operations."""

import pytest
from azure.core.exceptions import AzureError, HttpResponseError, ResourceNotFoundError, ServiceRequestError
from azure.storage.filedatalake import AccessControlChangeCounters, AccessControlChangeResult

from dlutil import (
    DEFAULT_DIR_ACL,
    DEFAULT_DIR_PERMISSIONS,
    DEFAULT_FILE_ACL,
    DEFAULT_FILE_PERMISSIONS,
    SUPERUSER,
    acl_set,
    assert_error,
    assert_status,
    put_file,
)

GUID = "4cf4e284-f6a8-4540-b53e-c3469af032dc"
ACL_RWX_RX_RWX = "user::rwx,group::r-x,other::rwx"
# remove-spec used by Microsoft's tests (entries need not exist)
REMOVE_ACL = (
    "mask,"
    "default:user,default:group,"
    "user:ec3595d6-2c17-4696-8caa-7e139758d24a,group:ec3595d6-2c17-4696-8caa-7e139758d24a,"
    "default:user:ec3595d6-2c17-4696-8caa-7e139758d24a,default:group:ec3595d6-2c17-4696-8caa-7e139758d24a"
)


def make_tree(fs, root="top", dirs=5, files=5):
    top = fs.create_directory(root)
    for i in range(dirs):
        sub = top.create_sub_directory(f"subdir{i}")
        for j in range(files):
            sub.create_file(f"subfile{j}")
    return top


# -- defaults and get_access_control ----------------------------------------
def test_new_file_default_access_control(fs):
    ac = fs.create_file("f").get_access_control()
    assert ac["owner"] == SUPERUSER
    assert ac["group"] == SUPERUSER
    assert ac["permissions"] == DEFAULT_FILE_PERMISSIONS
    assert ac["acl"] == DEFAULT_FILE_ACL


def test_new_directory_default_access_control(fs):
    ac = fs.create_directory("d").get_access_control()
    assert ac["owner"] == SUPERUSER
    assert ac["group"] == SUPERUSER
    assert ac["permissions"] == DEFAULT_DIR_PERMISSIONS
    assert ac["acl"] == DEFAULT_DIR_ACL


def test_get_access_control_with_upn(fs):
    ac = fs.create_file("f").get_access_control(upn=True)
    assert ac["owner"] == SUPERUSER
    assert ac["permissions"] == DEFAULT_FILE_PERMISSIONS


def test_implicit_parent_directory_has_default_access_control(fs):
    fs.create_file("p/q/f")
    ac = fs.get_directory_client("p/q").get_access_control()
    assert ac["owner"] == SUPERUSER
    assert ac["permissions"] == DEFAULT_DIR_PERMISSIONS


@pytest.mark.docs
def test_get_access_control_of_missing_path_is_404(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").get_access_control()
    assert_status(e, 404)


# -- set_access_control ------------------------------------------------------
def test_set_permissions_octal_on_file(fs):
    """Recorded: setAccessControl with x-ms-permissions 0777 -> rwxrwxrwx (no umask on set)."""
    f = fs.create_file("f")
    resp = f.set_access_control(permissions="0777")
    assert resp is not None
    ac = f.get_access_control()
    assert ac["permissions"] == "rwxrwxrwx"
    assert ac["acl"] == "user::rwx,group::rwx,other::rwx"


def test_set_permissions_octal_on_directory(fs):
    d = fs.create_directory("d")
    d.set_access_control(permissions="0777")
    ac = d.get_access_control()
    assert ac["permissions"] == "rwxrwxrwx"
    assert ac["acl"] == "user::rwx,group::rwx,other::rwx"


@pytest.mark.parametrize(
    "perm,expected_perm,expected_acl",
    [
        ("0644", "rw-r--r--", "user::rw-,group::r--,other::r--"),
        ("0750", "rwxr-x---", "user::rwx,group::r-x,other::---"),
        ("rwxr--r--", "rwxr--r--", "user::rwx,group::r--,other::r--"),
        ("0000", "---------", "user::---,group::---,other::---"),
    ],
)
@pytest.mark.docs
def test_set_permissions_forms(fs, perm, expected_perm, expected_acl):
    f = fs.create_file("f")
    f.set_access_control(permissions=perm)
    ac = f.get_access_control()
    assert ac["permissions"] == expected_perm
    assert ac["acl"] == expected_acl


def test_set_acl_on_file_updates_permissions(fs):
    f = fs.create_file("f")
    f.set_access_control(acl=ACL_RWX_RX_RWX)
    ac = f.get_access_control()
    assert ac["acl"] == ACL_RWX_RX_RWX
    assert ac["permissions"] == "rwxr-xrwx"


def test_set_acl_on_directory_updates_permissions(fs):
    """Recorded: acl user::rwx,group::r-x,other::rwx -> permissions rwxr-xrwx."""
    d = fs.create_directory("d", metadata={"hello": "world"})
    d.set_access_control(acl=ACL_RWX_RX_RWX)
    ac = d.get_access_control()
    assert ac["acl"] == ACL_RWX_RX_RWX
    assert ac["permissions"] == "rwxr-xrwx"


def test_set_access_control_returns_etag_and_last_modified(fs):
    d = fs.create_directory("d")
    resp = d.set_access_control(permissions="0777")
    assert resp["etag"]
    assert resp["last_modified"] is not None


def test_set_access_control_does_not_change_etag(fs):
    """Recorded (4 recordings): the ETag after setAccessControl equals the one before."""
    f = fs.create_file("f")
    before = f.get_file_properties().etag
    f.set_access_control(permissions="0777")
    assert f.get_file_properties().etag == before


def test_set_access_control_does_not_touch_data_or_metadata(fs):
    f = put_file(fs, "f", b"abc", metadata={"k": "v"})
    f.set_access_control(permissions="0777")
    props = f.get_file_properties()
    assert props.size == 3
    assert props.metadata == {"k": "v"}


def test_set_owner_and_group(fs):
    f = fs.create_file("f")
    f.set_access_control(owner=GUID, group=GUID)
    ac = f.get_access_control()
    assert ac["owner"] == GUID
    assert ac["group"] == GUID


def test_set_owner_only_keeps_group(fs):
    f = fs.create_file("f")
    f.set_access_control(owner=GUID)
    ac = f.get_access_control()
    assert ac["owner"] == GUID
    assert ac["group"] == SUPERUSER


def test_set_permissions_keeps_owner(fs):
    f = fs.create_file("f")
    f.set_access_control(owner=GUID)
    f.set_access_control(permissions="0777")
    assert f.get_access_control()["owner"] == GUID


def test_properties_report_owner_group_permissions_acl_after_set(fs):
    f = fs.create_file("f")
    f.set_access_control(owner=GUID, group=GUID, acl=ACL_RWX_RX_RWX)
    props = f.get_file_properties()
    assert props.owner == GUID
    assert props.group == GUID
    assert props.permissions == "rwxr-xrwx"
    assert props.acl == ACL_RWX_RX_RWX


def test_listing_reports_changed_permissions(fs):
    f = fs.create_file("f")
    f.set_access_control(permissions="0777")
    [p] = list(fs.get_paths())
    assert p.permissions == "rwxrwxrwx"


@pytest.mark.docs
def test_set_access_control_on_missing_path_is_404(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").set_access_control(permissions="0777")
    assert_status(e, 404)


@pytest.mark.docs
def test_set_access_control_invalid_acl_is_400(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.set_access_control(acl="this is not an acl")
    assert_status(e, 400)
    assert f.get_access_control()["acl"] == DEFAULT_FILE_ACL


@pytest.mark.docs
def test_set_access_control_invalid_permissions_is_400(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.set_access_control(permissions="9999")
    assert_status(e, 400)


@pytest.mark.docs
def test_set_access_control_with_named_user_entry_and_mask(fs):
    f = fs.create_file("f")
    acl = f"user::rw-,user:{GUID}:r--,group::r--,mask::r--,other::---"
    f.set_access_control(acl=acl)
    assert acl_set(f.get_access_control()["acl"]) == acl_set(acl)


# -- sticky bit (Microsoft docs: 4-digit octal first digit 1, or t/T in the last place) -------------
@pytest.mark.parametrize(
    "perm,expected",
    [
        ("1777", "rwxrwxrwt"),
        ("1766", "rwxrw-rwT"),
        ("0777", "rwxrwxrwx"),
        ("rwxrwxrwt", "rwxrwxrwt"),
        ("rwxrw-rwT", "rwxrw-rwT"),
    ],
)
@pytest.mark.docs
def test_sticky_bit_on_directory(fs, perm, expected):
    d = fs.create_directory("d")
    d.set_access_control(permissions=perm)
    assert d.get_access_control()["permissions"] == expected
    assert d.get_directory_properties().permissions == expected


@pytest.mark.docs
def test_sticky_bit_is_cleared_by_setting_plain_permissions(fs):
    d = fs.create_directory("d")
    d.set_access_control(permissions="1777")
    d.set_access_control(permissions="0777")
    assert d.get_access_control()["permissions"] == "rwxrwxrwx"


@pytest.mark.docs
def test_sticky_bit_set_at_creation_and_listed(fs):
    fs.get_directory_client("d").create_directory(permissions="1777", umask="0000")
    [p] = list(fs.get_paths())
    assert p.permissions == "rwxrwxrwt"


# -- create with permissions / umask / owner / acl ---------------------------
def test_create_directory_permissions_and_umask_zero(fs):
    """Recorded: permissions rwxr--r-- + umask 0000 -> acl user::rwx,group::r--,other::r--."""
    d = fs.get_directory_client("d")
    d.create_directory(permissions="rwxr--r--", umask="0000")
    ac = d.get_access_control()
    assert ac["permissions"] == "rwxr--r--"
    assert ac["acl"] == "user::rwx,group::r--,other::r--"


def test_create_directory_permissions_0777_umask_0000(fs):
    d = fs.get_directory_client("d")
    d.create_directory(permissions="0777", umask="0000")
    ac = d.get_access_control()
    assert ac["permissions"] == "rwxrwxrwx"
    assert ac["acl"] == "user::rwx,group::rwx,other::rwx"


def test_create_directory_permissions_0777_default_umask_applies_027(fs):
    """Recorded: x-ms-permissions 0777 without umask -> rwxr-x--- (default umask 027)."""
    d = fs.get_directory_client("d")
    d.create_directory(permissions="0777", metadata={"hello": "world", "number": "42"})
    ac = d.get_access_control()
    assert ac["permissions"] == "rwxr-x---"
    assert ac["acl"] == "user::rwx,group::r-x,other::---"


def test_create_directory_owner_group_acl(fs):
    """Recorded: owner/group GUIDs and an explicit acl are stored as given."""
    d = fs.get_directory_client("d")
    d.create_directory(owner=GUID, group=GUID, acl=ACL_RWX_RX_RWX)
    ac = d.get_access_control()
    assert ac["owner"] == GUID
    assert ac["group"] == GUID
    assert ac["acl"] == ACL_RWX_RX_RWX
    assert ac["permissions"] == "rwxr-xrwx"


def test_create_file_owner_group_acl(fs):
    f = fs.get_file_client("f")
    f.create_file(owner=GUID, group=GUID, acl=ACL_RWX_RX_RWX)
    ac = f.get_access_control()
    assert ac["owner"] == GUID
    assert ac["group"] == GUID
    assert ac["acl"] == ACL_RWX_RX_RWX


def test_create_file_permissions_0777_umask_0000(fs):
    f = fs.get_file_client("f")
    f.create_file(permissions="0777", umask="0000")
    assert f.get_access_control()["permissions"] == "rwxrwxrwx"


@pytest.mark.docs
def test_create_file_permissions_0777_default_umask(fs):
    f = fs.get_file_client("f")
    f.create_file(permissions="0777")
    assert f.get_access_control()["permissions"] == "rwxr-x---"


@pytest.mark.docs
def test_create_file_permissions_0666_umask_0022(fs):
    f = fs.get_file_client("f")
    f.create_file(permissions="0666", umask="0022")
    assert f.get_access_control()["permissions"] == "rw-r--r--"


@pytest.mark.docs
def test_create_file_umask_alone_applies_to_default_mode(fs):
    f = fs.get_file_client("f")
    f.create_file(umask="0077")
    assert f.get_access_control()["permissions"] == "rw-------"


@pytest.mark.docs
def test_create_sub_directory_permissions_and_umask(fs):
    d = fs.create_directory("d")
    sub = d.create_sub_directory("s", permissions="0755", umask="0000")
    assert sub.get_access_control()["permissions"] == "rwxr-xr-x"


# -- default ACL inheritance (not recorded; POSIX / Azure ACL documentation) -
@pytest.mark.docs
def test_default_acl_is_inherited_by_new_child_file(fs):
    d = fs.create_directory("d")
    d.set_access_control(
        acl="user::rwx,group::r-x,other::---,default:user::rwx,default:group::rwx,default:other::r-x"
    )
    child = d.create_file("child.txt")
    # files get the parent's default entries as their access ACL (masked by the requested mode 0666)
    assert acl_set(child.get_access_control()["acl"]) == {"user::rw-", "group::rw-", "other::r--"}


@pytest.mark.docs
def test_default_acl_is_inherited_by_new_child_directory_with_its_defaults(fs):
    d = fs.create_directory("d")
    default_part = "default:user::rwx,default:group::rwx,default:other::r-x"
    d.set_access_control(acl="user::rwx,group::r-x,other::---," + default_part)
    sub = d.create_sub_directory("sub")
    got = acl_set(sub.get_access_control()["acl"])
    assert {"user::rwx", "group::rwx", "other::r-x"} <= got
    assert set(default_part.split(",")) <= got


@pytest.mark.docs
def test_default_acl_is_inherited_by_nested_descendants(fs):
    d = fs.create_directory("d")
    default_part = "default:user::rwx,default:group::rwx,default:other::r-x"
    d.set_access_control(acl="user::rwx,group::r-x,other::---," + default_part)
    leaf = d.create_file("a/b/leaf.txt")
    assert acl_set(leaf.get_access_control()["acl"]) == {"user::rw-", "group::rw-", "other::r--"}
    assert set(default_part.split(",")) <= acl_set(fs.get_directory_client("d/a/b").get_access_control()["acl"])


@pytest.mark.docs
def test_default_acl_does_not_change_existing_children(fs):
    d = fs.create_directory("d")
    child = d.create_file("old.txt")
    before = child.get_access_control()["acl"]
    d.set_access_control(
        acl="user::rwx,group::r-x,other::---,default:user::rwx,default:group::rwx,default:other::r-x"
    )
    assert child.get_access_control()["acl"] == before


@pytest.mark.docs
def test_directory_without_default_acl_uses_umask_for_children(fs):
    d = fs.create_directory("d")
    assert d.create_file("f").get_access_control()["permissions"] == DEFAULT_FILE_PERMISSIONS
    assert d.create_sub_directory("s").get_access_control()["permissions"] == DEFAULT_DIR_PERMISSIONS


# -- recursive ACL operations (recorded) ------------------------------------
def assert_all_have_acl(fs, root, acl):
    for p in fs.get_paths(path=root):
        client = (
            fs.get_directory_client(p.name) if p.is_directory else fs.get_file_client(p.name)
        )
        assert client.get_access_control()["acl"] == acl, p.name


def test_set_access_control_recursive_counters(fs):
    top = make_tree(fs)
    summary = top.set_access_control_recursive(acl=ACL_RWX_RX_RWX)
    # the directory itself is counted too
    assert summary.counters.directories_successful == 6
    assert summary.counters.files_successful == 25
    assert summary.counters.failure_count == 0
    assert summary.continuation is None
    assert top.get_access_control()["acl"] == ACL_RWX_RX_RWX


def test_set_access_control_recursive_reaches_every_descendant(fs):
    top = make_tree(fs, dirs=2, files=2)
    top.set_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert_all_have_acl(fs, "top", ACL_RWX_RX_RWX)


def test_set_access_control_recursive_does_not_touch_siblings(fs):
    top = make_tree(fs, dirs=1, files=1)
    other = put_file(fs, "other/f", b"x")
    top.set_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert other.get_access_control()["acl"] == DEFAULT_FILE_ACL


def test_set_access_control_recursive_on_a_file(fs):
    f = fs.create_file("f")
    summary = f.set_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert summary.counters.files_successful == 1
    assert summary.counters.directories_successful == 0
    assert summary.counters.failure_count == 0
    assert f.get_access_control()["acl"] == ACL_RWX_RX_RWX


def test_set_access_control_recursive_on_empty_directory(fs):
    d = fs.create_directory("d")
    summary = d.set_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert summary.counters.directories_successful == 1
    assert summary.counters.files_successful == 0


def test_set_access_control_recursive_in_batches(fs):
    top = make_tree(fs)
    summary = top.set_access_control_recursive(acl=ACL_RWX_RX_RWX, batch_size=2)
    assert summary.counters.directories_successful == 6
    assert summary.counters.files_successful == 25
    assert summary.counters.failure_count == 0
    assert summary.continuation is None
    assert top.get_access_control()["acl"] == ACL_RWX_RX_RWX


def test_set_access_control_recursive_with_explicit_continuation(fs):
    top = make_tree(fs)
    tally = AccessControlChangeCounters(0, 0, 0)
    result = AccessControlChangeResult(None, "")
    iterations = 0
    while result.continuation is not None:
        result = top.set_access_control_recursive(
            acl=ACL_RWX_RX_RWX, batch_size=2, max_batches=2, continuation=result.continuation
        )
        tally.directories_successful += result.counters.directories_successful
        tally.files_successful += result.counters.files_successful
        tally.failure_count += result.counters.failure_count
        iterations += 1
        assert iterations < 50, "continuation never ended"
    assert iterations > 1  # 31 entries cannot fit in one 2 x 2 call
    assert tally.directories_successful == 6
    assert tally.files_successful == 25
    assert tally.failure_count == 0
    assert_all_have_acl(fs, "top", ACL_RWX_RX_RWX)


def test_set_access_control_recursive_max_batches_returns_continuation(fs):
    top = make_tree(fs)
    result = top.set_access_control_recursive(acl=ACL_RWX_RX_RWX, batch_size=2, max_batches=1)
    assert result.continuation
    assert result.counters.directories_successful + result.counters.files_successful == 2


def test_set_access_control_recursive_progress_hook(fs):
    top = make_tree(fs)
    batch_tally = AccessControlChangeCounters(0, 0, 0)
    last = []

    def hook(resp):
        batch_tally.directories_successful += resp.batch_counters.directories_successful
        batch_tally.files_successful += resp.batch_counters.files_successful
        batch_tally.failure_count += resp.batch_counters.failure_count
        last.append(resp.aggregate_counters)

    summary = top.set_access_control_recursive(acl=ACL_RWX_RX_RWX, progress_hook=hook, batch_size=2)
    assert summary.counters.directories_successful == batch_tally.directories_successful == 6
    assert summary.counters.files_successful == batch_tally.files_successful == 25
    assert summary.counters.failure_count == batch_tally.failure_count == 0
    assert last[-1].directories_successful == 6
    assert last[-1].files_successful == 25
    assert summary.continuation is None


def test_set_access_control_recursive_error_carries_continuation_token(fs):
    """Recorded: a transport error on the 2nd batch surfaces with a token to resume from."""
    top = make_tree(fs)
    seen = []

    def callback(response):
        seen.append(response)
        if len(seen) == 2:
            raise ServiceRequestError("network problem")

    with pytest.raises(AzureError) as err:
        top.set_access_control_recursive(
            acl=ACL_RWX_RX_RWX, batch_size=2, max_batches=2, raw_response_hook=callback, retry_total=0
        )
    assert err.value.continuation_token is not None
    assert err.value.message == "network problem"
    # resuming from the token finishes the job
    resumed = top.set_access_control_recursive(acl=ACL_RWX_RX_RWX, continuation=err.value.continuation_token)
    assert resumed.continuation is None
    assert_all_have_acl(fs, "top", ACL_RWX_RX_RWX)


def test_update_access_control_recursive_counters(fs):
    top = make_tree(fs)
    summary = top.update_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert summary.counters.directories_successful == 6
    assert summary.counters.files_successful == 25
    assert summary.counters.failure_count == 0
    assert top.get_access_control()["acl"] == ACL_RWX_RX_RWX


def test_update_access_control_recursive_in_batches(fs):
    top = make_tree(fs)
    summary = top.update_access_control_recursive(acl=ACL_RWX_RX_RWX, batch_size=2)
    assert summary.counters.directories_successful == 6
    assert summary.counters.files_successful == 25
    assert summary.continuation is None


def test_update_access_control_recursive_progress_hook(fs):
    top = make_tree(fs)
    tally = AccessControlChangeCounters(0, 0, 0)

    def hook(resp):
        tally.directories_successful += resp.batch_counters.directories_successful
        tally.files_successful += resp.batch_counters.files_successful

    top.update_access_control_recursive(acl=ACL_RWX_RX_RWX, progress_hook=hook, batch_size=2)
    assert (tally.directories_successful, tally.files_successful) == (6, 25)


def test_update_access_control_recursive_on_a_file(fs):
    f = fs.create_file("f")
    summary = f.update_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert summary.counters.files_successful == 1
    assert f.get_access_control()["acl"] == ACL_RWX_RX_RWX


@pytest.mark.docs
def test_update_access_control_recursive_modifies_only_given_entries(fs):
    top = make_tree(fs, dirs=1, files=1)
    top.update_access_control_recursive(acl="other::r--")
    f = fs.get_file_client("top/subdir0/subfile0")
    ac = f.get_access_control()
    assert ac["acl"] == "user::rw-,group::r--,other::r--"
    assert ac["permissions"] == "rw-r--r--"


def test_remove_access_control_recursive_counters(fs):
    top = make_tree(fs)
    summary = top.remove_access_control_recursive(acl=REMOVE_ACL)
    assert summary.counters.directories_successful == 6
    assert summary.counters.files_successful == 25
    assert summary.counters.failure_count == 0


def test_remove_access_control_recursive_in_batches(fs):
    top = make_tree(fs)
    summary = top.remove_access_control_recursive(acl=REMOVE_ACL, batch_size=2)
    assert summary.counters.directories_successful == 6
    assert summary.counters.files_successful == 25
    assert summary.continuation is None


def test_remove_access_control_recursive_progress_hook(fs):
    top = make_tree(fs)
    tally = AccessControlChangeCounters(0, 0, 0)

    def hook(resp):
        tally.directories_successful += resp.batch_counters.directories_successful
        tally.files_successful += resp.batch_counters.files_successful
        tally.failure_count += resp.batch_counters.failure_count

    summary = top.remove_access_control_recursive(acl=REMOVE_ACL, progress_hook=hook, batch_size=2)
    assert tally.directories_successful == summary.counters.directories_successful == 6
    assert tally.files_successful == summary.counters.files_successful == 25


def test_remove_access_control_recursive_on_a_file(fs):
    f = fs.create_file("f")
    summary = f.remove_access_control_recursive(acl=REMOVE_ACL)
    assert summary.counters.files_successful == 1
    assert summary.counters.failure_count == 0


@pytest.mark.docs
def test_recursive_acl_on_missing_directory_is_404(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_directory_client("nodir").set_access_control_recursive(acl=ACL_RWX_RX_RWX)
    assert_status(e, 404)
