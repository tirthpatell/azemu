"""Certificates: get, versions, policy, update, list and errors.

Sources:
  * Recorded: GET /certificates/{n}/ (latest) and /{n}/{version}; the 404 body
    {"error": {"code": "CertificateNotFound", "message": "A certificate with (name/id) <n> was
    not found in this key vault. If you recently deleted ..."}} and, for a missing version,
    "(name/id) <n>/versions/<v> was not found"; PATCH /certificates/{n}/ with {"tags": ...}
    returns the bundle with the same id and a new "updated"; GET and PATCH /certificates/{n}/policy
    (the response policy has the new values and a new policy "updated"); list items
    ({"id": no version, "x5t", "attributes": enabled/nbf/exp/created/updated, no recoveryLevel});
    list pages with maxresults (6 of 7 on the first page, the rest behind nextLink); list of
    versions with maxresults; get_certificate_version returns the same values as the list item
    plus recovery level.
  * From Azure docs (marked): invalid names and invalid policies -> 400 BadParameter (the same
    convention as the secrets and keys tests), tags replace on update, include_pending,
    a disabled certificate stays readable.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.certificates import (
    CertificateContentType,
    CertificatePolicy,
    CertificatePolicyAction,
    CertificateProperties,
    KeyCurveName,
    KeyType,
    KeyUsageType,
    KeyVaultCertificate,
    LifetimeAction,
)

from kvutil import begin_pending, create_cert, ec_policy, error_code, sha1_of, unique

HEX32 = re.compile(r"^[0-9a-f]{32}$")


# -- get_certificate ---------------------------------------------------------------
def test_get_certificate_latest_has_policy_and_cer(certs):
    name = unique("cert")
    created = create_cert(certs, name)
    got = certs.get_certificate(name)
    assert isinstance(got, KeyVaultCertificate)
    assert got.id == created.id
    assert got.cer == created.cer
    assert got.policy is not None
    assert got.policy.issuer_name == "Self"


def test_get_certificate_returns_the_newest_version(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    second = create_cert(certs, name, ec_policy())
    assert certs.get_certificate(name).properties.version == second.properties.version


def test_get_certificate_version_returns_that_version(certs):
    name = unique("cert")
    first = create_cert(certs, name, ec_policy())
    second = create_cert(certs, name, ec_policy())
    got = certs.get_certificate_version(name, first.properties.version)
    assert got.properties.version == first.properties.version
    assert got.cer == first.cer
    assert got.cer != second.cer
    assert got.id == first.id


def test_get_certificate_version_has_no_policy(certs):
    # Recorded: GET /certificates/{n}/{version} returns id, kid, sid, x5t, cer and attributes
    # only; the policy (and "pending") are only in the answer for the latest version.
    name = unique("cert")
    created = create_cert(certs, name, ec_policy("CN=versioned"))
    got = certs.get_certificate_version(name, created.properties.version)
    assert got.policy is None
    assert got.cer == created.cer


def test_get_certificate_properties_have_thumbprint_and_dates(certs):
    name = unique("cert")
    created = create_cert(certs, name)
    p = certs.get_certificate(name).properties
    assert p.x509_thumbprint == sha1_of(created.cer)
    assert p.not_before == created.properties.not_before
    assert p.expires_on == created.properties.expires_on
    assert p.created_on == created.properties.created_on
    assert p.updated_on == created.properties.updated_on
    assert p.recovery_level  # recorded: present in a get, absent in a list


# -- get_certificate_policy / update_certificate_policy --------------------------------
def test_get_certificate_policy_returns_the_create_policy(certs):
    # Recorded (test_policy): the returned policy equals what was sent.
    name = unique("cert")
    sent = CertificatePolicy(
        "Self",
        subject="CN=DefaultPolicy",
        exportable=True,
        key_type=KeyType.rsa,
        key_size=2048,
        reuse_key=True,
        enhanced_key_usage=["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"],
        key_usage=[KeyUsageType.decipher_only],
        content_type=CertificateContentType.pkcs12,
        validity_in_months=12,
        lifetime_actions=[LifetimeAction(action=CertificatePolicyAction.email_contacts, lifetime_percentage=98)],
        certificate_transparency=False,
        san_dns_names=["sdk.azure-int.net"],
    )
    certs.begin_create_certificate(name, sent, _polling_interval=0.1).wait()
    got = certs.get_certificate_policy(name)
    assert got.issuer_name == "Self"
    assert got.subject == "CN=DefaultPolicy"
    assert got.exportable is True
    assert got.key_type == KeyType.rsa
    assert got.key_size == 2048
    assert got.reuse_key is True
    assert set(got.enhanced_key_usage) == {"1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"}
    assert set(got.key_usage) == {KeyUsageType.decipher_only}
    assert got.content_type == CertificateContentType.pkcs12
    assert got.validity_in_months == 12
    assert got.certificate_transparency is False
    assert got.san_dns_names == ["sdk.azure-int.net"]
    assert [(a.action, a.lifetime_percentage, a.days_before_expiry) for a in got.lifetime_actions] == [
        (CertificatePolicyAction.email_contacts, 98, None)
    ]


def test_get_certificate_policy_has_attributes_dates(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    policy = certs.get_certificate_policy(name)
    assert policy.enabled is True
    assert abs(policy.created_on - datetime.now(timezone.utc)) < timedelta(minutes=2)
    assert policy.updated_on >= policy.created_on


def test_update_certificate_policy_changes_key_type_and_returns_it(certs):
    # Recorded (test_policy): RSA 2048 -> EC P-256 via PATCH /policy returns the new policy.
    name = unique("cert")
    certs.begin_create_certificate(name, CertificatePolicy.get_default(), _polling_interval=0.1).wait()
    new = CertificatePolicy(
        "Self",
        subject="CN=DefaultPolicy",
        exportable=True,
        key_type=KeyType.ec,
        key_size=256,
        key_curve_name=KeyCurveName.p_256,
        reuse_key=True,
        content_type=CertificateContentType.pkcs12,
        validity_in_months=12,
    )
    returned = certs.update_certificate_policy(name, new)
    assert returned.key_type == KeyType.ec
    assert returned.key_curve_name == KeyCurveName.p_256
    assert returned.reuse_key is True
    again = certs.get_certificate_policy(name)
    assert again.key_type == KeyType.ec
    assert again.key_curve_name == KeyCurveName.p_256


def test_update_certificate_policy_changes_lifetime_actions_and_validity(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    new = ec_policy(
        validity_in_months=24,
        lifetime_actions=[LifetimeAction(action=CertificatePolicyAction.auto_renew, days_before_expiry=45)],
    )
    returned = certs.update_certificate_policy(name, new)
    assert returned.validity_in_months == 24
    assert [(a.action, a.days_before_expiry) for a in returned.lifetime_actions] == [
        (CertificatePolicyAction.auto_renew, 45)
    ]


def test_update_certificate_policy_does_not_change_the_existing_certificate(certs):
    # From Azure docs: the policy governs the next certificate version, not the existing one.
    name = unique("cert")
    created = create_cert(certs, name, ec_policy("CN=old-subject"))
    certs.update_certificate_policy(name, ec_policy("CN=new-subject"))
    got = certs.get_certificate(name)
    assert got.cer == created.cer
    assert got.properties.version == created.properties.version
    assert certs.get_certificate_policy(name).subject == "CN=new-subject"


def test_update_certificate_policy_refreshes_policy_updated_date(certs):
    # Recorded: attributes.updated of the policy changes with PATCH /policy.
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    before = certs.get_certificate_policy(name)
    after = certs.update_certificate_policy(name, ec_policy(validity_in_months=6))
    assert after.updated_on >= before.updated_on
    assert after.created_on == before.created_on


def test_creating_a_new_version_with_another_policy_replaces_the_policy(certs):
    # From Azure docs: each create carries a policy; the certificate then has that policy.
    name = unique("cert")
    create_cert(certs, name, ec_policy("CN=first"))
    create_cert(certs, name, ec_policy("CN=second"))
    assert certs.get_certificate_policy(name).subject == "CN=second"


# -- versions ---------------------------------------------------------------------------
def test_second_create_makes_a_new_version_and_keeps_the_first_readable(certs):
    name = unique("cert")
    first = create_cert(certs, name, ec_policy())
    second = create_cert(certs, name, ec_policy())
    assert first.properties.version != second.properties.version
    assert HEX32.match(second.properties.version)
    assert first.id == f"{certs.vault_url}/certificates/{name}/{first.properties.version}"
    assert certs.get_certificate_version(name, first.properties.version).cer == first.cer


def test_list_certificate_versions_returns_every_version(certs):
    name = unique("cert")
    made = [create_cert(certs, name, ec_policy()) for _ in range(3)]
    listed = list(certs.list_properties_of_certificate_versions(name))
    assert sorted(p.version for p in listed) == sorted(c.properties.version for c in made)
    assert all(isinstance(p, CertificateProperties) for p in listed)
    assert all(p.name == name for p in listed)
    assert all(p.id == f"{certs.vault_url}/certificates/{name}/{p.version}" for p in listed)
    assert all(p.vault_url == certs.vault_url for p in listed)


def test_list_certificate_versions_items_equal_get_certificate_version(certs):
    # Recorded (test_get_certificate_version): list item and get give the same properties,
    # except that recoverable fields are only in the get.
    name = unique("cert")
    for _ in range(3):
        create_cert(certs, name, ec_policy())
    for item in certs.list_properties_of_certificate_versions(name):
        got = certs.get_certificate_version(name, item.version).properties
        assert item.created_on == got.created_on
        assert item.enabled == got.enabled
        assert item.expires_on == got.expires_on
        assert item.id == got.id
        assert item.name == got.name
        assert item.not_before == got.not_before
        assert item.tags == got.tags
        assert item.updated_on == got.updated_on
        assert item.vault_url == got.vault_url
        assert item.version == got.version
        assert item.x509_thumbprint == got.x509_thumbprint
        assert item.recovery_level is None  # recorded: not in list items


def test_list_certificate_versions_pages_with_max_page_size(certs):
    # Recorded (test_list_certificate_versions): maxresults=N gives N items, the rest behind nextLink.
    name = unique("cert")
    for _ in range(5):
        create_cert(certs, name, ec_policy())
    pages = [list(page) for page in certs.list_properties_of_certificate_versions(name, max_page_size=2).by_page()]
    assert [len(p) for p in pages] == [2, 2, 1]
    versions = [p.version for page in pages for p in page]
    assert len(set(versions)) == 5


def test_list_certificate_versions_of_unknown_name_is_empty(certs):
    # From Azure docs: listing versions of a name that does not exist returns an empty list.
    assert list(certs.list_properties_of_certificate_versions(unique("nope"))) == []


# -- list ---------------------------------------------------------------------------------
def test_list_properties_of_certificates_empty_vault(certs):
    assert list(certs.list_properties_of_certificates()) == []


def test_list_properties_of_certificates_returns_each_name_once_without_version(certs):
    a, b = unique("cert"), unique("cert")
    create_cert(certs, a, ec_policy())
    create_cert(certs, a, ec_policy())  # second version
    create_cert(certs, b, ec_policy())
    items = list(certs.list_properties_of_certificates())
    assert sorted(p.name for p in items) == sorted([a, b])
    assert all(isinstance(p, CertificateProperties) for p in items)
    assert all(p.version is None for p in items)  # recorded: list "id" has no version
    assert all(p.id == f"{certs.vault_url}/certificates/{p.name}" for p in items)
    assert all(p.vault_url == certs.vault_url for p in items)


def test_list_properties_of_certificates_item_values(certs):
    # Recorded list item: id, x5t, attributes enabled / nbf / exp / created / updated, no recoveryLevel.
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    (item,) = list(certs.list_properties_of_certificates())
    assert item.enabled is True
    assert item.x509_thumbprint == sha1_of(created.cer)
    assert item.not_before == created.properties.not_before
    assert item.expires_on == created.properties.expires_on
    assert item.created_on == created.properties.created_on
    assert item.recovery_level is None
    assert item.tags is None


def test_list_properties_of_certificates_pages_with_max_page_size(certs):
    # Recorded (test_list): maxresults=6 returns 6 of 7 and a nextLink with the 7th on the next page.
    names = {unique("cert") for _ in range(5)}
    for n in names:
        create_cert(certs, n, ec_policy())
    pages = [list(p) for p in certs.list_properties_of_certificates(max_page_size=2).by_page()]
    assert [len(p) for p in pages] == [2, 2, 1]
    assert {p.name for page in pages for p in page} == names


def test_list_properties_of_certificates_follows_pages_transparently(certs):
    names = {unique("cert") for _ in range(4)}
    for n in names:
        create_cert(certs, n, ec_policy())
    assert {p.name for p in certs.list_properties_of_certificates(max_page_size=3)} == names


def test_list_properties_of_certificates_includes_disabled_and_tags(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy(), enabled=False, tags={"a": "b"})
    (item,) = list(certs.list_properties_of_certificates())
    assert item.enabled is False
    assert item.tags == {"a": "b"}


def test_list_properties_of_certificates_leaves_out_pending_by_default(certs):
    # Spec: includePending "Specifies whether to include certificates which are not completely
    # provisioned." (no recording of a list with a pending certificate).
    done, pending = unique("cert"), unique("ext")
    create_cert(certs, done, ec_policy())
    begin_pending(certs, pending)
    assert {p.name for p in certs.list_properties_of_certificates()} == {done}
    assert {p.name for p in certs.list_properties_of_certificates(include_pending=True)} == {done, pending}


# -- update_certificate_properties -----------------------------------------------------------
def test_update_certificate_properties_tags_keep_the_version_and_the_certificate(certs):
    # Recorded (test_crud_operations): PATCH {"tags": ...} returns the same id; nothing else changes.
    name = unique("cert")
    created = create_cert(certs, name)
    updated = certs.update_certificate_properties(name, tags={"tag1": "updated_value1"})
    assert updated.properties.tags == {"tag1": "updated_value1"}
    assert updated.id == created.id
    assert updated.cer == created.cer
    assert updated.policy.subject == "CN=DefaultPolicy"
    assert updated.properties.created_on == created.properties.created_on
    assert updated.properties.updated_on >= created.properties.updated_on
    assert certs.get_certificate(name).properties.tags == {"tag1": "updated_value1"}


def test_update_certificate_properties_replaces_tags(certs):
    # From Azure docs: PATCH replaces the tag set (same behaviour as secrets and keys).
    name = unique("cert")
    create_cert(certs, name, ec_policy(), tags={"old": "1"})
    certs.update_certificate_properties(name, tags={"new": "2"})
    assert certs.get_certificate(name).properties.tags == {"new": "2"}


def test_update_certificate_properties_enabled_false_and_back(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    off = certs.update_certificate_properties(name, enabled=False)
    assert off.properties.enabled is False
    # From Azure docs: the metadata of a disabled certificate can still be read.
    got = certs.get_certificate(name)
    assert got.properties.enabled is False
    assert got.cer == created.cer
    on = certs.update_certificate_properties(name, enabled=True)
    assert on.properties.enabled is True


def test_update_certificate_properties_of_one_version_only(certs):
    name = unique("cert")
    first = create_cert(certs, name, ec_policy())
    second = create_cert(certs, name, ec_policy())
    certs.update_certificate_properties(name, first.properties.version, tags={"which": "first"})
    assert certs.get_certificate_version(name, first.properties.version).properties.tags == {"which": "first"}
    assert certs.get_certificate_version(name, second.properties.version).properties.tags is None


def test_update_certificate_properties_without_version_targets_the_latest(certs):
    name = unique("cert")
    first = create_cert(certs, name, ec_policy())
    second = create_cert(certs, name, ec_policy())
    certs.update_certificate_properties(name, tags={"which": "latest"})
    assert certs.get_certificate_version(name, second.properties.version).properties.tags == {"which": "latest"}
    assert certs.get_certificate_version(name, first.properties.version).properties.tags is None


# -- errors: not found ---------------------------------------------------------------------------
def test_get_missing_certificate_is_not_found_with_recorded_code_and_message(certs):
    name = unique("missing")
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_certificate(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "CertificateNotFound"
    assert f"A certificate with (name/id) {name} was not found in this key vault" in exc.value.message


def test_get_missing_certificate_version_is_not_found_with_version_in_message(certs):
    # Recorded: "(name/id) <name>/versions/<version> was not found".
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    ghost = "0" * 32
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_certificate_version(name, ghost)
    assert error_code(exc.value) == "CertificateNotFound"
    assert f"{name}/versions/{ghost}" in exc.value.message


def test_get_certificate_error_body_shape(certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_certificate(unique("missing"))
    assert exc.value.response.json()["error"]["code"] == "CertificateNotFound"
    assert isinstance(exc.value.response.json()["error"]["message"], str)


def test_missing_certificate_policy_update_and_delete_are_not_found(certs):
    # 404 and code CertificateNotFound are from Azure docs; only get is recorded.
    name = unique("missing")
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_certificate_policy(name)
    assert error_code(exc.value) == "CertificateNotFound"
    with pytest.raises(ResourceNotFoundError):
        certs.update_certificate_policy(name, ec_policy())
    with pytest.raises(ResourceNotFoundError):
        certs.update_certificate_properties(name, tags={"a": "b"})
    with pytest.raises(ResourceNotFoundError):
        certs.begin_delete_certificate(name)


def test_get_certificate_of_other_name_is_not_found(certs):
    create_cert(certs, unique("cert"), ec_policy())
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate(unique("other"))


# -- errors: invalid input ---------------------------------------------------------------------------
@pytest.mark.parametrize("bad_name", ["under_score", "dot.ted", "sp ace", "slash@sign"])
def test_create_certificate_with_invalid_name_is_bad_parameter(certs, bad_name):
    # Spec: certificate-name must match ^[0-9a-zA-Z-]+$. The code is from Azure docs
    # (no invalid-name case in the recordings), as for secrets and keys.
    with pytest.raises(HttpResponseError) as exc:
        certs.begin_create_certificate(bad_name, ec_policy())
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_get_certificate_with_invalid_name_is_bad_parameter(certs):
    with pytest.raises(HttpResponseError) as exc:
        certs.get_certificate("not_valid")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_name_with_digits_letters_and_dashes_is_valid(certs):
    name = "Az-09-" + unique("Mixed")
    assert create_cert(certs, name, ec_policy()).name == name


def test_create_with_unsupported_rsa_key_size_is_bad_parameter(certs):
    # Vault RSA sizes are 2048, 3072 and 4096 (spec, Azure docs); 1000 is not valid.
    policy = CertificatePolicy("Self", subject="CN=bad-size", key_type=KeyType.rsa, key_size=1000)
    with pytest.raises(HttpResponseError) as exc:
        certs.begin_create_certificate(unique("cert"), policy)
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_create_with_invalid_subject_is_bad_parameter(certs):
    # From Azure docs: the subject must be an X.500 distinguished name.
    policy = CertificatePolicy("Self", subject="this is not a distinguished name", key_type="EC", key_curve_name="P-256")
    with pytest.raises(HttpResponseError) as exc:
        certs.begin_create_certificate(unique("cert"), policy)
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_failed_create_leaves_no_certificate_behind(certs):
    name = unique("cert")
    policy = CertificatePolicy("Self", subject="CN=bad-size", key_type=KeyType.rsa, key_size=1000)
    with pytest.raises(HttpResponseError):
        certs.begin_create_certificate(name, policy)
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate(name)
    assert list(certs.list_properties_of_certificates(include_pending=True)) == []
