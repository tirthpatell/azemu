"""Secrets: set / get / versions / list / update / errors.

Expected behaviour comes from Azure Key Vault recordings made by Microsoft's own
SDK tests (refs/recordings/.../azure-keyvault-secrets) and the Secrets REST spec.
Items marked "from Azure docs" are not in the recordings.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.secrets import SecretProperties

from kvutil import delete_and_wait, error_code, unique

HEX32 = re.compile(r"^[0-9a-f]{32}$")
RECOVERY_LEVELS = {
    "Purgeable",
    "Recoverable+Purgeable",
    "Recoverable",
    "Recoverable+ProtectedSubscription",
    "CustomizedRecoverable+Purgeable",
    "CustomizedRecoverable",
    "CustomizedRecoverable+ProtectedSubscription",
}


def _utcnow():
    return datetime.now(timezone.utc)


# -- set / get ---------------------------------------------------------------
def test_set_secret_returns_name_and_value(secrets):
    name = unique("sec")
    secret = secrets.set_secret(name, "s3cret-value")
    assert secret.name == name
    assert secret.value == "s3cret-value"


def test_set_secret_id_is_vault_url_secrets_name_version(secrets):
    name = unique("sec")
    secret = secrets.set_secret(name, "v")
    version = secret.properties.version
    assert HEX32.match(version), version
    assert secret.id == f"{secrets.vault_url}/secrets/{name}/{version}"
    assert secret.properties.id == secret.id


def test_set_secret_vault_url_property_matches_the_client_vault(secrets):
    secret = secrets.set_secret(unique("sec"), "v")
    assert secret.properties.vault_url == secrets.vault_url


def test_set_secret_defaults_are_enabled_and_no_optional_metadata(secrets):
    secret = secrets.set_secret(unique("sec"), "v")
    p = secret.properties
    assert p.enabled is True
    assert p.content_type is None
    assert p.tags is None
    assert p.not_before is None
    assert p.expires_on is None


def test_set_secret_with_content_type_and_tags(secrets):
    name = unique("sec")
    secret = secrets.set_secret(name, "v", content_type="text/plain", tags={"a": "1", "b": "two"})
    assert secret.properties.content_type == "text/plain"
    assert secret.properties.tags == {"a": "1", "b": "two"}
    fetched = secrets.get_secret(name)
    assert fetched.properties.content_type == "text/plain"
    assert fetched.properties.tags == {"a": "1", "b": "two"}


def test_set_secret_not_before_and_expires_on_round_trip(secrets):
    name = unique("sec")
    nbf = datetime(2015, 2, 3, 0, 0, 0, tzinfo=timezone.utc)
    exp = datetime(2050, 1, 1, 0, 0, 0, tzinfo=timezone.utc)
    secrets.set_secret(name, "v", not_before=nbf, expires_on=exp)
    props = secrets.get_secret(name).properties
    assert props.not_before == nbf
    assert props.expires_on == exp


def test_set_secret_enabled_false_is_stored(secrets):
    name = unique("sec")
    secret = secrets.set_secret(name, "v", enabled=False)
    assert secret.properties.enabled is False


def test_set_secret_created_on_and_updated_on_are_recent_utc(secrets):
    before = _utcnow() - timedelta(minutes=2)
    secret = secrets.set_secret(unique("sec"), "v")
    after = _utcnow() + timedelta(minutes=2)
    p = secret.properties
    assert before <= p.created_on <= after
    assert before <= p.updated_on <= after
    assert p.created_on.tzinfo is not None


def test_set_secret_recovery_level_and_recoverable_days(secrets):
    p = secrets.set_secret(unique("sec"), "v").properties
    assert p.recovery_level in RECOVERY_LEVELS
    assert "Recoverable" in p.recovery_level  # soft delete is always on in current Key Vault
    assert 7 <= p.recoverable_days <= 90


def test_get_secret_returns_latest_value(secrets):
    name = unique("sec")
    secrets.set_secret(name, "one")
    secrets.set_secret(name, "two")
    assert secrets.get_secret(name).value == "two"


def test_get_secret_latest_matches_set_secret_version(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "v")
    fetched = secrets.get_secret(name)
    assert fetched.properties.version == created.properties.version
    assert fetched.id == created.id


def test_get_secret_specific_version(secrets):
    name = unique("sec")
    first = secrets.set_secret(name, "first")
    second = secrets.set_secret(name, "second")
    assert first.properties.version != second.properties.version
    assert secrets.get_secret(name, first.properties.version).value == "first"
    assert secrets.get_secret(name, second.properties.version).value == "second"


def test_set_secret_same_value_twice_still_creates_a_new_version(secrets):
    name = unique("sec")
    a = secrets.set_secret(name, "same")
    b = secrets.set_secret(name, "same")
    assert a.properties.version != b.properties.version


def test_get_secret_unicode_value_round_trip(secrets):
    name = unique("sec")
    value = "pässwörd-日本語-\U0001f512-line1\nline2\ttab"
    secrets.set_secret(name, value)
    assert secrets.get_secret(name).value == value


def test_get_secret_json_value_is_returned_verbatim(secrets):
    name = unique("sec")
    value = '{"user": "a\\"b", "n": [1, 2, 3]}'
    secrets.set_secret(name, value)
    assert secrets.get_secret(name).value == value


def test_get_secret_near_size_limit_round_trip(secrets):
    # Key Vault allows secret values up to 25 KiB (Azure docs).
    name = unique("sec")
    value = "x" * (20 * 1024)
    secrets.set_secret(name, value)
    assert secrets.get_secret(name).value == value


def test_set_secret_value_over_25_kib_is_rejected(secrets):
    # From Azure docs: the maximum secret size is 25 KiB. The exact error code is
    # not in the recordings, so only the 4xx status is asserted.
    with pytest.raises(HttpResponseError) as exc:
        secrets.set_secret(unique("sec"), "x" * (30 * 1024))
    assert 400 <= exc.value.status_code < 500


def test_get_secret_out_content_type_pem_on_plain_secret_is_bad_parameter(secrets):
    # Recorded: a non-certificate secret cannot be converted to PEM.
    name = unique("sec")
    secrets.set_secret(name, "plain")
    with pytest.raises(HttpResponseError) as exc:
        secrets.get_secret(name, out_content_type="application/x-pem-file")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"
    assert "PEM" in exc.value.message


# -- errors ------------------------------------------------------------------
def test_get_missing_secret_is_secret_not_found(secrets):
    name = unique("missing")
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_secret(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "SecretNotFound"
    assert name in exc.value.message
    assert "was not found in this key vault" in exc.value.message


def test_get_missing_version_of_existing_secret_is_secret_not_found(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_secret(name, "0" * 32)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "SecretNotFound"


def test_update_missing_secret_is_secret_not_found(secrets):
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.update_secret_properties(unique("missing"), tags={"a": "b"})
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "SecretNotFound"


def test_get_disabled_secret_is_forbidden(secrets):
    # Recorded for the same engine: 403, code "Forbidden",
    # message "Operation get is not allowed on a disabled secret."
    name = unique("sec")
    secrets.set_secret(name, "v", enabled=False)
    with pytest.raises(HttpResponseError) as exc:
        secrets.get_secret(name)
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"
    assert "disabled secret" in exc.value.message


def test_disabled_secret_can_be_read_again_after_enabling(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v", enabled=False)
    secrets.update_secret_properties(name, enabled=True)
    assert secrets.get_secret(name).value == "v"


@pytest.mark.parametrize("bad_name", ["under_score", "dot.ted", "sp ace", "slash@sign", "pct%sign"])
def test_set_secret_with_invalid_name_is_bad_parameter(secrets, bad_name):
    # Spec: secret-name must match ^[0-9a-zA-Z-]+$. Error code from Azure docs
    # (no invalid-name case in the recordings).
    with pytest.raises(HttpResponseError) as exc:
        secrets.set_secret(bad_name, "v")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_get_secret_with_invalid_name_is_bad_parameter(secrets):
    with pytest.raises(HttpResponseError) as exc:
        secrets.get_secret("not_valid")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_name_with_digits_letters_and_dashes_is_valid(secrets):
    name = "Az-09-" + unique("Mixed")
    assert secrets.set_secret(name, "v").name == name


def test_error_body_has_error_code_and_message(secrets):
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_secret(unique("missing"))
    err = exc.value.error
    assert err is not None
    assert err.code and err.message


# -- versions ----------------------------------------------------------------
def test_list_secret_versions_returns_every_version(secrets):
    name = unique("sec")
    versions = {secrets.set_secret(name, f"v{i}").properties.version for i in range(4)}
    listed = list(secrets.list_properties_of_secret_versions(name))
    assert len(listed) == 4
    assert {p.version for p in listed} == versions
    assert all(isinstance(p, SecretProperties) for p in listed)
    assert all(p.name == name for p in listed)


def test_list_secret_versions_ids_end_with_the_version(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "v")
    (listed,) = list(secrets.list_properties_of_secret_versions(name))
    assert listed.id == created.id
    assert listed.vault_url == secrets.vault_url


def test_list_secret_versions_honours_max_page_size(secrets):
    name = unique("sec")
    versions = {secrets.set_secret(name, f"v{i}").properties.version for i in range(5)}
    pages = [list(pg) for pg in secrets.list_properties_of_secret_versions(name, max_page_size=2).by_page()]
    assert all(1 <= len(pg) <= 2 for pg in pages)
    assert len(pages) >= 3
    assert {p.version for pg in pages for p in pg} == versions


def test_list_secret_versions_of_unknown_secret_is_empty(secrets):
    # Recorded: GET /secrets/<name>/versions on a name that does not exist is a
    # 200 with {"value": [], "nextLink": null}.
    assert list(secrets.list_properties_of_secret_versions(unique("missing"))) == []


def test_list_secret_versions_do_not_include_values(secrets):
    name = unique("sec")
    secrets.set_secret(name, "hidden")
    (p,) = list(secrets.list_properties_of_secret_versions(name))
    assert not hasattr(p, "value")


# -- list --------------------------------------------------------------------
def test_list_properties_of_secrets_empty_vault(secrets):
    assert list(secrets.list_properties_of_secrets()) == []


def test_list_properties_of_secrets_returns_each_secret_once(secrets):
    a, b = unique("sec"), unique("sec")
    secrets.set_secret(a, "1")
    secrets.set_secret(a, "2")  # second version of the same secret
    secrets.set_secret(b, "3")
    names = sorted(p.name for p in secrets.list_properties_of_secrets())
    assert names == sorted([a, b])


def test_list_properties_of_secrets_items_have_no_value_and_no_version(secrets):
    # Recorded: list items carry "id": ".../secrets/<name>" without a version.
    name = unique("sec")
    secrets.set_secret(name, "hidden", content_type="text/plain", tags={"k": "v"})
    (p,) = list(secrets.list_properties_of_secrets())
    assert isinstance(p, SecretProperties)
    assert not hasattr(p, "value")
    assert p.name == name
    assert p.version is None
    assert p.id == f"{secrets.vault_url}/secrets/{name}"
    assert p.vault_url == secrets.vault_url
    assert p.content_type == "text/plain"
    assert p.tags == {"k": "v"}
    assert p.enabled is True


def test_list_properties_of_secrets_includes_disabled_secrets(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v", enabled=False)
    (p,) = list(secrets.list_properties_of_secrets())
    assert p.name == name
    assert p.enabled is False


def test_list_properties_of_secrets_pages_through_more_than_25(secrets):
    # Spec: the service returns up to 25 results per page and a nextLink.
    names = {unique("sec") for _ in range(30)}
    for n in names:
        secrets.set_secret(n, "v")
    listed = [p.name for p in secrets.list_properties_of_secrets()]
    assert len(listed) == 30
    assert set(listed) == names


def test_list_properties_of_secrets_first_page_is_at_most_25(secrets):
    for _ in range(27):
        secrets.set_secret(unique("sec"), "v")
    pages = [list(pg) for pg in secrets.list_properties_of_secrets().by_page()]
    assert len(pages) >= 2
    assert all(len(pg) <= 25 for pg in pages)
    assert sum(len(pg) for pg in pages) == 27


def test_list_properties_of_secrets_honours_max_page_size(secrets):
    names = {unique("sec") for _ in range(7)}
    for n in names:
        secrets.set_secret(n, "v")
    pages = [list(pg) for pg in secrets.list_properties_of_secrets(max_page_size=3).by_page()]
    assert all(len(pg) <= 3 for pg in pages)
    assert len(pages) >= 3
    assert {p.name for pg in pages for p in pg} == names


def test_list_properties_of_secrets_skips_soft_deleted_secrets(secrets):
    keep, gone = unique("keep"), unique("gone")
    secrets.set_secret(keep, "v")
    secrets.set_secret(gone, "v")
    delete_and_wait(secrets, gone)
    assert [p.name for p in secrets.list_properties_of_secrets()] == [keep]


# -- update ------------------------------------------------------------------
def test_update_secret_properties_changes_metadata_but_not_value(secrets):
    name = unique("sec")
    secrets.set_secret(name, "keep-me", tags={"old": "1"})
    exp = datetime(2040, 5, 6, 7, 8, 9, tzinfo=timezone.utc)
    updated = secrets.update_secret_properties(
        name, content_type="application/json", tags={"new": "2"}, expires_on=exp
    )
    assert updated.content_type == "application/json"
    assert updated.tags == {"new": "2"}
    assert updated.expires_on == exp
    assert updated.enabled is True
    assert secrets.get_secret(name).value == "keep-me"


def test_update_secret_properties_disable_makes_get_forbidden(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    assert secrets.update_secret_properties(name, enabled=False).enabled is False
    with pytest.raises(HttpResponseError) as exc:
        secrets.get_secret(name)
    assert exc.value.status_code == 403


def test_update_secret_properties_returns_properties_with_same_version(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "v")
    updated = secrets.update_secret_properties(name, tags={"a": "b"})
    assert isinstance(updated, SecretProperties)
    assert updated.version == created.properties.version
    assert updated.name == name


def test_update_secret_properties_does_not_create_a_new_version(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    secrets.update_secret_properties(name, tags={"a": "b"})
    secrets.update_secret_properties(name, tags={"a": "c"})
    assert len(list(secrets.list_properties_of_secret_versions(name))) == 1


def test_update_secret_properties_targets_the_requested_version(secrets):
    name = unique("sec")
    first = secrets.set_secret(name, "one")
    second = secrets.set_secret(name, "two")
    secrets.update_secret_properties(name, first.properties.version, tags={"which": "first"})
    assert secrets.get_secret(name, first.properties.version).properties.tags == {"which": "first"}
    assert secrets.get_secret(name, second.properties.version).properties.tags is None
    assert secrets.get_secret(name).properties.tags is None


def test_update_secret_properties_defaults_to_latest_version(secrets):
    name = unique("sec")
    first = secrets.set_secret(name, "one")
    second = secrets.set_secret(name, "two")
    secrets.update_secret_properties(name, tags={"which": "latest"})
    assert secrets.get_secret(name, second.properties.version).properties.tags == {"which": "latest"}
    assert secrets.get_secret(name, first.properties.version).properties.tags is None


def test_update_secret_properties_keeps_created_on_and_moves_updated_on_forward(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "v").properties
    updated = secrets.update_secret_properties(name, tags={"a": "b"})
    assert updated.created_on == created.created_on
    assert updated.updated_on >= created.updated_on
