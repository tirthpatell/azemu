"""Keys: create / get / versions / list / update / import / rotate / rotation policy.

Ground truth: recordings in refs/recordings/.../azure-keyvault-keys (vault runs, not
Managed HSM) and the Keys REST spec. Recorded facts used here:
  * default RSA key_ops: encrypt, decrypt, sign, verify, wrapKey, unwrapKey
  * default EC key_ops: sign, verify; default curve P-256
  * JWK "n", "e", "x", "y" are base64url; the SDK decodes them to bytes
  * list items carry "kid" without a version
  * the service strips leading zero bytes from an imported key's "n"
  * restore / recover keep the original kid
Not offered by vaults per the recordings: plain "oct" keys (only oct-HSM) and
get_random_bytes (Managed HSM only); neither is tested.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.keys import (
    JsonWebKey,
    KeyCurveName,
    KeyOperation,
    KeyProperties,
    KeyRotationLifetimeAction,
    KeyRotationPolicy,
    KeyRotationPolicyAction,
    KeyType,
    KeyVaultKey,
)

from kvutil import (
    EC_COORD_BYTES,
    ec_jwk_kwargs,
    error_code,
    generate_ec_private_key,
    generate_rsa_private_key,
    int_from_bytes,
    rsa_jwk_kwargs,
    unique,
)

HEX32 = re.compile(r"^[0-9a-f]{32}$")
ALL_RSA_OPS = {"encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"}


def _ops(key):
    return {str(getattr(o, "value", o)) for o in key.key_operations}


# -- create RSA --------------------------------------------------------------
def test_create_rsa_key_default_is_2048_bits_with_exponent_65537(keys):
    name = unique("rsa")
    key = keys.create_rsa_key(name)
    assert isinstance(key, KeyVaultKey)
    assert key.name == name
    assert key.key_type == KeyType.rsa
    assert len(key.key.n) == 256
    assert int_from_bytes(key.key.e) == 65537
    assert key.key.d is None  # private material is never returned


@pytest.mark.parametrize("size", [2048, 3072, 4096])
def test_create_rsa_key_size(keys, size):
    key = keys.create_rsa_key(unique("rsa"), size=size)
    assert key.key_type == KeyType.rsa
    assert int_from_bytes(key.key.n).bit_length() == size


def test_create_rsa_key_kid_has_name_and_version(keys):
    name = unique("rsa")
    key = keys.create_rsa_key(name)
    version = key.properties.version
    assert HEX32.match(version), version
    assert key.id == f"{keys.vault_url}/keys/{name}/{version}"
    assert key.key.kid == key.id
    assert key.properties.vault_url == keys.vault_url


def test_create_rsa_key_default_key_operations(keys):
    key = keys.create_rsa_key(unique("rsa"))
    assert _ops(key) == ALL_RSA_OPS


def test_create_rsa_key_with_requested_key_operations(keys):
    key = keys.create_rsa_key(unique("rsa"), key_operations=["encrypt", "decrypt"])
    assert _ops(key) == {"encrypt", "decrypt"}


def test_create_rsa_key_with_tags_enabled_and_dates(keys):
    name = unique("rsa")
    nbf = datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    exp = datetime(2060, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    created = keys.create_rsa_key(name, tags={"purpose": "unit test"}, not_before=nbf, expires_on=exp)
    for props in (created.properties, keys.get_key(name).properties):
        assert props.tags == {"purpose": "unit test"}
        assert props.enabled is True
        assert props.not_before == nbf
        assert props.expires_on == exp


def test_create_rsa_key_disabled(keys):
    key = keys.create_rsa_key(unique("rsa"), enabled=False)
    assert key.properties.enabled is False


def test_create_key_generic_rsa(keys):
    key = keys.create_key(unique("gen"), "RSA", size=2048)
    assert key.key_type == KeyType.rsa
    assert len(key.key.n) == 256


def test_create_rsa_key_same_name_makes_new_version(keys):
    name = unique("rsa")
    first = keys.create_rsa_key(name)
    second = keys.create_rsa_key(name)
    assert first.properties.version != second.properties.version
    assert first.key.n != second.key.n
    assert keys.get_key(name).properties.version == second.properties.version


# -- create EC ---------------------------------------------------------------
@pytest.mark.parametrize("curve", ["P-256", "P-384", "P-521", "P-256K"])
def test_create_ec_key_curve(keys, curve):
    key = keys.create_ec_key(unique("ec"), curve=curve)
    assert key.key_type == KeyType.ec
    assert str(getattr(key.key.crv, "value", key.key.crv)) == curve
    assert len(key.key.x) == EC_COORD_BYTES[curve]
    assert len(key.key.y) == EC_COORD_BYTES[curve]
    assert key.key.d is None


def test_create_ec_key_default_curve_is_p256(keys):
    key = keys.create_ec_key(unique("ec"))
    assert str(getattr(key.key.crv, "value", key.key.crv)) == "P-256"


def test_create_ec_key_default_key_operations_are_sign_and_verify(keys):
    key = keys.create_ec_key(unique("ec"))
    assert _ops(key) == {"sign", "verify"}


def test_create_ec_key_with_curve_enum_and_tags(keys):
    key = keys.create_ec_key(unique("ec"), curve=KeyCurveName.p_384, tags={"t": "1"})
    assert str(getattr(key.key.crv, "value", key.key.crv)) == "P-384"
    assert key.properties.tags == {"t": "1"}


def test_create_ec_key_public_point_is_on_the_curve(keys):
    from kvutil import ec_public_key_from_jwk

    key = keys.create_ec_key(unique("ec"), curve="P-256")
    ec_public_key_from_jwk(key.key)  # raises ValueError if the point is not on the curve


# -- get ---------------------------------------------------------------------
def test_get_key_returns_same_public_material_as_create(keys):
    name = unique("rsa")
    created = keys.create_rsa_key(name)
    fetched = keys.get_key(name)
    assert fetched.id == created.id
    assert fetched.key.n == created.key.n
    assert fetched.key.e == created.key.e
    assert fetched.key.kty == created.key.kty
    assert fetched.key.d is None


def test_get_key_specific_version(keys):
    name = unique("rsa")
    first = keys.create_rsa_key(name)
    second = keys.create_rsa_key(name)
    assert keys.get_key(name, first.properties.version).key.n == first.key.n
    assert keys.get_key(name, second.properties.version).key.n == second.key.n


def test_get_key_properties(keys):
    name = unique("ec")
    before = datetime.now(timezone.utc) - timedelta(minutes=2)
    keys.create_ec_key(name)
    p = keys.get_key(name).properties
    assert isinstance(p, KeyProperties)
    assert p.name == name
    assert HEX32.match(p.version)
    assert p.vault_url == keys.vault_url
    assert p.enabled is True
    assert before <= p.created_on <= datetime.now(timezone.utc) + timedelta(minutes=2)
    assert p.updated_on >= p.created_on
    assert "Recoverable" in p.recovery_level
    assert 7 <= p.recoverable_days <= 90


def test_get_key_jwk_kid_matches_key_id(keys):
    key = keys.create_ec_key(unique("ec"))
    fetched = keys.get_key(key.name)
    assert fetched.key.kid == fetched.id


def test_get_key_key_operations_round_trip(keys):
    name = unique("rsa")
    keys.create_rsa_key(name, key_operations=["sign", "verify"])
    assert _ops(keys.get_key(name)) == {"sign", "verify"}


def test_get_missing_key_is_key_not_found(keys):
    name = unique("missing")
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.get_key(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "KeyNotFound"
    assert name in exc.value.message
    assert "was not found in this key vault" in exc.value.message


def test_get_missing_version_of_existing_key_is_key_not_found(keys):
    name = unique("ec")
    keys.create_ec_key(name)
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.get_key(name, "0" * 32)
    assert error_code(exc.value) == "KeyNotFound"


@pytest.mark.parametrize("bad_name", ["under_score", "dot.ted", "sp ace"])
def test_create_key_with_invalid_name_is_bad_parameter(keys, bad_name):
    # Spec: key-name must match ^[0-9a-zA-Z-]+$. Error code from Azure docs.
    with pytest.raises(HttpResponseError) as exc:
        keys.create_ec_key(bad_name)
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_create_rsa_key_with_unsupported_size_is_bad_parameter(keys):
    # Vault RSA sizes are 2048, 3072 and 4096 (Azure docs); 1000 is not valid.
    with pytest.raises(HttpResponseError) as exc:
        keys.create_rsa_key(unique("rsa"), size=1000)
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


# -- list --------------------------------------------------------------------
def test_list_properties_of_keys_empty_vault(keys):
    assert list(keys.list_properties_of_keys()) == []


def test_list_properties_of_keys_returns_each_key_once_without_material(keys):
    a, b = unique("ec"), unique("ec")
    keys.create_ec_key(a, tags={"k": "v"})
    keys.create_ec_key(a)  # second version
    keys.create_ec_key(b)
    items = list(keys.list_properties_of_keys())
    assert sorted(p.name for p in items) == sorted([a, b])
    assert all(isinstance(p, KeyProperties) for p in items)
    assert all(p.version is None for p in items)  # recorded: list "kid" has no version
    assert all(p.id == f"{keys.vault_url}/keys/{p.name}" for p in items)
    assert all(p.vault_url == keys.vault_url for p in items)


def test_list_properties_of_keys_includes_disabled_and_tags(keys):
    name = unique("ec")
    keys.create_ec_key(name, enabled=False, tags={"a": "b"})
    (p,) = list(keys.list_properties_of_keys())
    assert p.enabled is False
    assert p.tags == {"a": "b"}


def test_list_properties_of_keys_pages_through_more_than_25(keys):
    names = {unique("ec") for _ in range(30)}
    for n in names:
        keys.create_ec_key(n)
    pages = [list(pg) for pg in keys.list_properties_of_keys().by_page()]
    assert len(pages) >= 2
    assert all(len(pg) <= 25 for pg in pages)
    assert {p.name for pg in pages for p in pg} == names


def test_list_properties_of_keys_honours_max_page_size(keys):
    names = {unique("ec") for _ in range(7)}
    for n in names:
        keys.create_ec_key(n)
    pages = [list(pg) for pg in keys.list_properties_of_keys(max_page_size=3).by_page()]
    assert all(len(pg) <= 3 for pg in pages)
    assert {p.name for pg in pages for p in pg} == names


def test_list_properties_of_key_versions(keys):
    name = unique("ec")
    versions = {keys.create_ec_key(name).properties.version for _ in range(4)}
    listed = list(keys.list_properties_of_key_versions(name))
    assert {p.version for p in listed} == versions
    assert len(listed) == 4
    assert all(p.name == name for p in listed)


def test_list_properties_of_key_versions_honours_max_page_size(keys):
    name = unique("ec")
    versions = {keys.create_ec_key(name).properties.version for _ in range(5)}
    pages = [list(pg) for pg in keys.list_properties_of_key_versions(name, max_page_size=2).by_page()]
    assert all(len(pg) <= 2 for pg in pages)
    assert len(pages) >= 3
    assert {p.version for pg in pages for p in pg} == versions


def test_list_properties_of_key_versions_of_unknown_key_is_empty(keys):
    assert list(keys.list_properties_of_key_versions(unique("missing"))) == []


# -- update ------------------------------------------------------------------
def test_update_key_properties_changes_metadata_not_key_material(keys):
    name = unique("rsa")
    created = keys.create_rsa_key(name, tags={"old": "1"})
    exp = datetime(2050, 6, 7, 8, 9, 10, tzinfo=timezone.utc)
    updated = keys.update_key_properties(name, key_operations=["decrypt", "encrypt"], tags={"foo": "updated tag"}, expires_on=exp)
    assert _ops(updated) == {"decrypt", "encrypt"}
    assert updated.properties.tags == {"foo": "updated tag"}
    assert updated.properties.expires_on == exp
    assert updated.properties.version == created.properties.version
    assert updated.key.n == created.key.n


def test_update_key_properties_key_operations_visible_on_get(keys):
    name = unique("rsa")
    keys.create_rsa_key(name)
    keys.update_key_properties(name, key_operations=["verify"])
    assert _ops(keys.get_key(name)) == {"verify"}


def test_update_key_properties_enabled_false_then_true(keys):
    name = unique("ec")
    keys.create_ec_key(name)
    assert keys.update_key_properties(name, enabled=False).properties.enabled is False
    assert keys.get_key(name).properties.enabled is False
    assert keys.update_key_properties(name, enabled=True).properties.enabled is True


def test_update_key_properties_does_not_create_a_version(keys):
    name = unique("ec")
    keys.create_ec_key(name)
    keys.update_key_properties(name, tags={"a": "1"})
    keys.update_key_properties(name, tags={"a": "2"})
    assert len(list(keys.list_properties_of_key_versions(name))) == 1


def test_update_key_properties_targets_requested_version(keys):
    name = unique("ec")
    first = keys.create_ec_key(name)
    second = keys.create_ec_key(name)
    keys.update_key_properties(name, first.properties.version, tags={"w": "first"})
    assert keys.get_key(name, first.properties.version).properties.tags == {"w": "first"}
    assert keys.get_key(name, second.properties.version).properties.tags is None


def test_update_key_properties_defaults_to_latest_version(keys):
    name = unique("ec")
    first = keys.create_ec_key(name)
    second = keys.create_ec_key(name)
    keys.update_key_properties(name, tags={"w": "latest"})
    assert keys.get_key(name, second.properties.version).properties.tags == {"w": "latest"}
    assert keys.get_key(name, first.properties.version).properties.tags is None


def test_update_missing_key_is_key_not_found(keys):
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.update_key_properties(unique("missing"), tags={"a": "b"})
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "KeyNotFound"


# -- import ------------------------------------------------------------------
def test_import_key_rsa_returns_public_part_only(keys):
    name = unique("imp")
    private = generate_rsa_private_key(2048)
    jwk = JsonWebKey(**rsa_jwk_kwargs(private, key_ops=["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"]))
    imported = keys.import_key(name, jwk)
    pub = private.public_key().public_numbers()
    assert imported.name == name
    assert imported.key_type == KeyType.rsa
    assert int_from_bytes(imported.key.n) == pub.n  # leading zero bytes are stripped by the service
    assert int_from_bytes(imported.key.e) == pub.e
    assert imported.key.d is None and imported.key.p is None and imported.key.q is None
    assert _ops(imported) == ALL_RSA_OPS
    assert HEX32.match(imported.properties.version)


def test_import_key_rsa_can_be_fetched_with_get_key(keys):
    name = unique("imp")
    private = generate_rsa_private_key(2048)
    keys.import_key(name, JsonWebKey(**rsa_jwk_kwargs(private, key_ops=["sign", "verify"])))
    fetched = keys.get_key(name)
    assert int_from_bytes(fetched.key.n) == private.public_key().public_numbers().n
    assert _ops(fetched) == {"sign", "verify"}


@pytest.mark.parametrize("curve", ["P-256", "P-384", "P-521", "P-256K"])
def test_import_key_ec_returns_public_point(keys, curve):
    # EC import is derived from the spec (JsonWebKey crv/x/y/d); the recordings only import RSA.
    name = unique("imp")
    private = generate_ec_private_key(curve)
    imported = keys.import_key(name, JsonWebKey(**ec_jwk_kwargs(private, curve, key_ops=["sign", "verify"])))
    pub = private.public_key().public_numbers()
    assert imported.key_type == KeyType.ec
    assert str(getattr(imported.key.crv, "value", imported.key.crv)) == curve
    assert int_from_bytes(imported.key.x) == pub.x
    assert int_from_bytes(imported.key.y) == pub.y
    assert imported.key.d is None
    assert _ops(imported) == {"sign", "verify"}


def test_import_key_with_tags_and_disabled(keys):
    name = unique("imp")
    private = generate_ec_private_key("P-256")
    imported = keys.import_key(
        name, JsonWebKey(**ec_jwk_kwargs(private, "P-256", key_ops=["sign", "verify"])), tags={"src": "local"}, enabled=False
    )
    assert imported.properties.tags == {"src": "local"}
    assert imported.properties.enabled is False


def test_import_key_over_existing_name_creates_a_new_version(keys):
    name = unique("imp")
    first = keys.import_key(name, JsonWebKey(**ec_jwk_kwargs(generate_ec_private_key("P-256"), "P-256", key_ops=["sign", "verify"])))
    second = keys.import_key(name, JsonWebKey(**ec_jwk_kwargs(generate_ec_private_key("P-256"), "P-256", key_ops=["sign", "verify"])))
    assert first.properties.version != second.properties.version
    assert len(list(keys.list_properties_of_key_versions(name))) == 2


def test_import_key_without_key_material_is_bad_parameter(keys):
    # Assumed 400: an RSA JWK without n/e is not a key. Only the status is asserted.
    with pytest.raises(HttpResponseError) as exc:
        keys.import_key(unique("imp"), JsonWebKey(kty="RSA", key_ops=["sign"]))
    assert exc.value.status_code == 400


# -- rotate ------------------------------------------------------------------
def test_rotate_key_creates_a_new_version_with_new_material(keys):
    name = unique("rot")
    original = keys.create_rsa_key(name)
    rotated = keys.rotate_key(name)
    assert isinstance(rotated, KeyVaultKey)
    assert rotated.name == name
    assert rotated.properties.version != original.properties.version
    assert rotated.key.n != original.key.n
    assert rotated.key_type == KeyType.rsa
    assert len(rotated.key.n) == len(original.key.n)
    assert keys.get_key(name).properties.version == rotated.properties.version


def test_rotate_key_keeps_key_operations_and_older_version(keys):
    name = unique("rot")
    original = keys.create_rsa_key(name, key_operations=["sign", "verify"])
    rotated = keys.rotate_key(name)
    assert _ops(rotated) == {"sign", "verify"}
    assert len(list(keys.list_properties_of_key_versions(name))) == 2
    assert keys.get_key(name, original.properties.version).key.n == original.key.n


def test_rotate_ec_key_keeps_curve(keys):
    name = unique("rot")
    keys.create_ec_key(name, curve="P-384")
    rotated = keys.rotate_key(name)
    assert str(getattr(rotated.key.crv, "value", rotated.key.crv)) == "P-384"


def test_rotate_missing_key_is_key_not_found(keys):
    # Assumed: the usual KeyNotFound 404.
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.rotate_key(unique("missing"))
    assert error_code(exc.value) == "KeyNotFound"


# -- rotation policy ---------------------------------------------------------
def test_update_key_rotation_policy_rotate_after_create_round_trip(keys):
    name = unique("rot")
    keys.create_rsa_key(name)
    action = KeyRotationLifetimeAction(KeyRotationPolicyAction.rotate, time_after_create="P2M")
    updated = keys.update_key_rotation_policy(name, KeyRotationPolicy(), lifetime_actions=[action], expires_in="P90D")
    assert isinstance(updated, KeyRotationPolicy)
    assert updated.id == f"{keys.vault_url}/keys/{name}/rotationpolicy"
    assert updated.expires_in == "P90D"
    rotate = [a for a in updated.lifetime_actions if str(getattr(a.action, "value", a.action)).lower() == "rotate"]
    assert len(rotate) == 1
    assert rotate[0].time_after_create == "P2M"

    fetched = keys.get_key_rotation_policy(name)
    assert fetched.id == updated.id
    assert fetched.expires_in == "P90D"
    rotate = [a for a in fetched.lifetime_actions if str(getattr(a.action, "value", a.action)).lower() == "rotate"]
    assert len(rotate) == 1 and rotate[0].time_after_create == "P2M"
    assert fetched.created_on is not None and fetched.updated_on is not None


def test_update_key_rotation_policy_notify_before_expiry_replaces_actions(keys):
    name = unique("rot")
    keys.create_rsa_key(name)
    rotate = KeyRotationLifetimeAction(KeyRotationPolicyAction.rotate, time_after_create="P2M")
    keys.update_key_rotation_policy(name, KeyRotationPolicy(), lifetime_actions=[rotate], expires_in="P90D")
    notify = KeyRotationLifetimeAction(KeyRotationPolicyAction.notify, time_before_expiry="P60D")
    updated = keys.update_key_rotation_policy(name, KeyRotationPolicy(), lifetime_actions=[notify], expires_in="P90D")
    kinds = [str(getattr(a.action, "value", a.action)).lower() for a in updated.lifetime_actions]
    assert kinds == ["notify"]
    assert updated.lifetime_actions[0].time_before_expiry == "P60D"
    assert updated.expires_in == "P90D"


def test_get_key_rotation_policy_on_new_key_returns_a_policy(keys):
    name = unique("rot")
    keys.create_rsa_key(name)
    policy = keys.get_key_rotation_policy(name)
    assert isinstance(policy, KeyRotationPolicy)
    assert policy.id == f"{keys.vault_url}/keys/{name}/rotationpolicy"


def test_rotation_policy_of_missing_key_is_key_not_found(keys):
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.get_key_rotation_policy(unique("missing"))
    assert error_code(exc.value) == "KeyNotFound"
