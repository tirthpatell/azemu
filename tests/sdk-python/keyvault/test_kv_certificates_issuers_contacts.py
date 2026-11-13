"""Certificates: issuers, contacts and the certificate operation (cancel / delete).

Sources:
  * Recorded issuers: PUT /certificates/issuers/{n} with {"provider", "credentials": {"account_id"},
    "org_details": {"admin_details": [...]}, "attributes": {"enabled"}} -> 200 with the same plus
    attributes created / updated, and "org_details": {"zip": 0, ...} whenever org_details was sent;
    a provider-only issuer comes back without credentials and org_details; GET -> 200; list items
    are {"id", "provider"}; PATCH with only org_details keeps provider and account_id; DELETE -> 200
    with the deleted issuer; GET of a missing issuer -> 404 CertificateIssuerNotFound "Issuer not found".
  * Recorded contacts: PUT /certificates/contacts -> 200 {"id": ".../certificates/contacts",
    "contacts": [...]}; GET; DELETE -> 200 with the deleted contacts; GET when none -> 404
    ContactsNotFound "Contacts not found".
  * Recorded operation: PATCH .../pending {"cancellation_requested": true} -> 200 with
    cancellation_requested true; DELETE .../pending -> 200 with the operation; afterwards GET
    .../pending -> 404 PendingCertificateNotFound "Pending Certificate not found: <name>"
    (recordings from 2022 and 2023 spell it "Pending certificate not found", with a small c: the wording changed over time, not with the API version).
  * From Azure docs / assumed (marked): password and organization id round trip, issuer update of
    single fields, replacing contacts, errors for missing issuers and invalid names.
"""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.certificates import (
    AdministratorContact,
    CertificateContact,
    CertificateIssuer,
    CertificateOperation,
    IssuerProperties,
)

from kvutil import begin_pending, create_cert, ec_policy, error_code, unique, unknown_policy


def _admin(first="John", last="Doe", email="admin@example.test", phone="4255555555"):
    return AdministratorContact(first_name=first, last_name=last, email=email, phone=phone)


def _issuer_url(certs, name):
    return f"{certs.vault_url}/certificates/issuers/{name}"


# -- issuers ----------------------------------------------------------------------------------------
def test_create_issuer_minimal_provider_only(certs):
    # Recorded (test_logging_enabled): {"provider": "Test"} -> issuer with provider and enabled attributes.
    name = unique("iss")
    issuer = certs.create_issuer(name, "Test")
    assert isinstance(issuer, CertificateIssuer)
    assert issuer.name == name
    assert issuer.provider == "Test"
    assert issuer.id == _issuer_url(certs, name)
    assert issuer.enabled is True
    assert issuer.account_id is None
    assert not issuer.admin_contacts


def test_create_issuer_with_account_and_admin_contacts(certs):
    # Recorded (test_crud_issuer).
    name = unique("iss")
    issuer = certs.create_issuer(
        name, "Test", account_id="keyvaultuser", admin_contacts=[_admin()], enabled=True
    )
    assert issuer.provider == "Test"
    assert issuer.account_id == "keyvaultuser"
    assert issuer.enabled is True
    assert len(issuer.admin_contacts) == 1
    c = issuer.admin_contacts[0]
    assert (c.first_name, c.last_name, c.email, c.phone) == ("John", "Doe", "admin@example.test", "4255555555")
    assert issuer.created_on is not None
    assert issuer.updated_on is not None


def test_create_issuer_with_several_admin_contacts(certs):
    name = unique("iss")
    contacts = [_admin("A", "One", "a@example.test", "111"), _admin("B", "Two", "b@example.test", "222")]
    issuer = certs.create_issuer(name, "Test", admin_contacts=contacts)
    assert {(c.first_name, c.email) for c in issuer.admin_contacts} == {("A", "a@example.test"), ("B", "b@example.test")}


def test_create_issuer_with_password_and_organization_id(certs):
    # From the REST spec (credentials.pwd, org_details.id); the SDK sends them. The password is
    # accepted; whether it is returned is not asserted.
    name = unique("iss")
    issuer = certs.create_issuer(
        name, "Test", account_id="acct", password="s3cret-pw", organization_id="org-123", admin_contacts=[_admin()]
    )
    assert issuer.account_id == "acct"
    assert issuer.organization_id == "org-123"
    assert certs.get_issuer(name).organization_id == "org-123"


def test_create_issuer_disabled(certs):
    # From Azure docs: the enabled attribute of the issuer.
    name = unique("iss")
    assert certs.create_issuer(name, "Test", enabled=False).enabled is False
    assert certs.get_issuer(name).enabled is False


def test_get_issuer_returns_what_was_created(certs):
    name = unique("iss")
    certs.create_issuer(name, "Test", account_id="keyvaultuser", admin_contacts=[_admin()])
    got = certs.get_issuer(name)
    assert got.name == name
    assert got.provider == "Test"
    assert got.account_id == "keyvaultuser"
    assert got.admin_contacts[0].email == "admin@example.test"
    assert got.id == _issuer_url(certs, name)


def test_create_issuer_twice_replaces_it(certs):
    # PUT semantics (Azure docs: "Sets the specified certificate issuer").
    name = unique("iss")
    certs.create_issuer(name, "Test", account_id="one")
    certs.create_issuer(name, "Test", account_id="two")
    assert certs.get_issuer(name).account_id == "two"
    assert len(list(certs.list_properties_of_issuers())) == 1


def test_list_properties_of_issuers_items(certs):
    # Recorded: list items hold only id and provider.
    a, b = unique("iss"), unique("iss")
    certs.create_issuer(a, "Test", account_id="x", admin_contacts=[_admin()])
    certs.create_issuer(b, "Test")
    items = list(certs.list_properties_of_issuers())
    assert all(isinstance(i, IssuerProperties) for i in items)
    assert {i.name for i in items} == {a, b}
    assert all(i.provider == "Test" for i in items)
    assert {i.id for i in items} == {_issuer_url(certs, a), _issuer_url(certs, b)}


def test_list_properties_of_issuers_empty(certs):
    assert list(certs.list_properties_of_issuers()) == []


def test_list_properties_of_issuers_pages_with_max_page_size(certs):
    # The page size parameter is in the spec (maxresults); the list itself is recorded.
    names = {unique("iss") for _ in range(3)}
    for n in names:
        certs.create_issuer(n, "Test")
    pages = [list(p) for p in certs.list_properties_of_issuers(max_page_size=2).by_page()]
    assert [len(p) for p in pages] == [2, 1]
    assert {i.name for page in pages for i in page} == names


def test_update_issuer_admin_contacts_keeps_provider_and_account(certs):
    # Recorded (test_crud_issuer): PATCH with only org_details; provider and account_id stay.
    name = unique("iss")
    certs.create_issuer(name, "Test", account_id="keyvaultuser", admin_contacts=[_admin("John")])
    updated = certs.update_issuer(name, admin_contacts=[_admin("Jane")])
    assert updated.provider == "Test"
    assert updated.account_id == "keyvaultuser"
    assert [c.first_name for c in updated.admin_contacts] == ["Jane"]
    assert [c.first_name for c in certs.get_issuer(name).admin_contacts] == ["Jane"]


def test_update_issuer_account_id_and_enabled(certs):
    # From Azure docs: PATCH changes only the fields that are sent.
    name = unique("iss")
    certs.create_issuer(name, "Test", account_id="before", admin_contacts=[_admin()])
    updated = certs.update_issuer(name, account_id="after", enabled=False)
    assert updated.account_id == "after"
    assert updated.enabled is False
    assert updated.provider == "Test"
    assert updated.admin_contacts[0].email == "admin@example.test"


def test_update_issuer_changes_updated_date_but_not_created(certs):
    name = unique("iss")
    first = certs.create_issuer(name, "Test", account_id="x")
    second = certs.update_issuer(name, account_id="y")
    assert second.created_on == first.created_on
    assert second.updated_on >= first.updated_on


def test_delete_issuer_returns_the_issuer_and_removes_it(certs):
    # Recorded: DELETE -> 200 with the deleted issuer; later GET -> 404 CertificateIssuerNotFound.
    name = unique("iss")
    certs.create_issuer(name, "Test", account_id="keyvaultuser", admin_contacts=[_admin()])
    deleted = certs.delete_issuer(name)
    assert isinstance(deleted, CertificateIssuer)
    assert deleted.name == name
    assert deleted.account_id == "keyvaultuser"
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_issuer(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "CertificateIssuerNotFound"
    assert exc.value.error.message == "Issuer not found"
    assert list(certs.list_properties_of_issuers()) == []


def test_get_missing_issuer_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_issuer(unique("missing"))
    assert error_code(exc.value) == "CertificateIssuerNotFound"
    assert exc.value.error.message == "Issuer not found"


def test_update_and_delete_missing_issuer_are_not_found(certs):
    # 404 is certain; the error code is assumed to be the same as for GET.
    name = unique("missing")
    with pytest.raises(ResourceNotFoundError):
        certs.update_issuer(name, account_id="x")
    with pytest.raises(ResourceNotFoundError):
        certs.delete_issuer(name)


@pytest.mark.parametrize("bad_name", ["under_score", "sp ace", "dot.ted"])
def test_create_issuer_with_invalid_name_is_bad_parameter(certs, bad_name):
    # Spec: issuer-name must match ^[0-9a-zA-Z-]+$ (code assumed, as for the other names).
    with pytest.raises(HttpResponseError) as exc:
        certs.create_issuer(bad_name, "Test")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_issuers_are_per_vault(certs, make_clients):
    _s, _k, other = make_clients("issuervault2")
    name = unique("iss")
    certs.create_issuer(name, "Test")
    assert list(other.list_properties_of_issuers()) == []
    with pytest.raises(ResourceNotFoundError):
        other.get_issuer(name)


# -- contacts ----------------------------------------------------------------------------------------------
def _contacts():
    return [
        CertificateContact(email="admin@contoso.example.test", name="John Doe", phone="1111111111"),
        CertificateContact(email="admin2@contoso.example.test", name="John Doe2", phone="2222222222"),
    ]


def _as_tuples(contacts):
    return sorted((c.email, c.name, c.phone) for c in contacts)


def test_set_contacts_returns_them(certs):
    # Recorded (test_crud_contacts).
    contacts = certs.set_contacts(_contacts())
    assert _as_tuples(contacts) == _as_tuples(_contacts())
    assert all(isinstance(c, CertificateContact) for c in contacts)


def test_get_contacts_returns_what_was_set(certs):
    certs.set_contacts(_contacts())
    assert _as_tuples(certs.get_contacts()) == _as_tuples(_contacts())


def test_delete_contacts_returns_the_deleted_contacts(certs):
    certs.set_contacts(_contacts())
    deleted = certs.delete_contacts()
    assert _as_tuples(deleted) == _as_tuples(_contacts())


def test_get_contacts_after_delete_is_not_found(certs):
    # Recorded: 404, code ContactsNotFound, message "Contacts not found".
    certs.set_contacts(_contacts())
    certs.delete_contacts()
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_contacts()
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "ContactsNotFound"
    assert exc.value.error.message == "Contacts not found"


def test_get_contacts_when_none_were_ever_set_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_contacts()
    assert error_code(exc.value) == "ContactsNotFound"


def test_set_contacts_again_replaces_the_list(certs):
    # From Azure docs: PUT replaces the contacts of the vault.
    certs.set_contacts(_contacts())
    only = [CertificateContact(email="only@example.test", name="Only One", phone="3")]
    certs.set_contacts(only)
    assert _as_tuples(certs.get_contacts()) == _as_tuples(only)


def test_set_contacts_with_email_only(certs):
    # name and phone are optional in the spec.
    got = certs.set_contacts([CertificateContact(email="x@example.test")])
    assert [(c.email, c.name, c.phone) for c in got] == [("x@example.test", None, None)]


def test_delete_contacts_when_none_is_not_found(certs):
    # Assumed to be the same error as GET.
    with pytest.raises(ResourceNotFoundError):
        certs.delete_contacts()


def test_contacts_are_per_vault(certs, make_clients):
    _s, _k, other = make_clients("contactsvault2")
    certs.set_contacts(_contacts())
    with pytest.raises(ResourceNotFoundError):
        other.get_contacts()


def test_contacts_error_body_shape(certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_contacts()
    assert exc.value.response.json() == {"error": {"code": "ContactsNotFound", "message": "Contacts not found"}}


# -- certificate operation: cancel and delete ------------------------------------------------------------------
def test_cancel_certificate_operation_returns_cancellation_requested(certs):
    # Recorded: PATCH .../pending {"cancellation_requested": true} -> the operation with cancellation_requested true.
    name = unique("ext")
    begin_pending(certs, name)
    op = certs.cancel_certificate_operation(name)
    assert isinstance(op, CertificateOperation)
    assert op.cancellation_requested is True
    assert op.id == f"{certs.vault_url}/certificates/{name}/pending"
    assert op.issuer_name == "Unknown"
    assert op.csr


def test_get_certificate_operation_after_cancel_still_says_cancellation_requested(certs):
    name = unique("ext")
    begin_pending(certs, name)
    certs.cancel_certificate_operation(name)
    got = certs.get_certificate_operation(name)
    assert got.cancellation_requested is True
    assert got.id == f"{certs.vault_url}/certificates/{name}/pending"


def test_delete_certificate_operation_returns_it_and_removes_it(certs):
    # Recorded: DELETE .../pending -> 200 with the operation; then GET .../pending -> 404
    # PendingCertificateNotFound.
    name = unique("ext")
    begin_pending(certs, name)
    certs.cancel_certificate_operation(name)
    deleted = certs.delete_certificate_operation(name)
    assert isinstance(deleted, CertificateOperation)
    assert deleted.id == f"{certs.vault_url}/certificates/{name}/pending"
    assert deleted.csr
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_certificate_operation(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "PendingCertificateNotFound"
    assert f"pending certificate not found: {name}".lower() in exc.value.message.lower()


def test_delete_operation_of_a_pending_certificate_without_cancelling_first(certs):
    name = unique("ext")
    begin_pending(certs, name)
    certs.delete_certificate_operation(name)
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate_operation(name)


def test_cancelled_pending_certificate_can_still_be_deleted(certs):
    # Recorded (test_async_request_cancellation_and_deletion): cancel, delete the operation, then
    # delete the certificate.
    from kvutil import delete_and_wait

    name = unique("ext")
    begin_pending(certs, name)
    certs.cancel_certificate_operation(name)
    certs.delete_certificate_operation(name)
    deleted = delete_and_wait(certs, name)
    assert deleted.name == name


def test_cancel_missing_certificate_operation_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError):
        certs.cancel_certificate_operation(unique("missing"))


def test_get_missing_certificate_operation_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate_operation(unique("missing"))


def test_delete_missing_certificate_operation_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError):
        certs.delete_certificate_operation(unique("missing"))


def test_operation_fields_of_a_pending_certificate(certs):
    name = unique("ext")
    begin_pending(certs, name, unknown_policy("CN=op-fields"))
    op = certs.get_certificate_operation(name)
    assert op.name == name
    assert op.vault_url == certs.vault_url
    assert op.request_id
    assert op.status.lower() == "inprogress"
    assert op.error is None


def test_two_creates_of_the_same_pending_name_are_independent_vault_wide(certs):
    # A pending operation of one name does not disturb another name.
    a, b = unique("ext"), unique("cert")
    begin_pending(certs, a)
    create_cert(certs, b, ec_policy())
    assert certs.get_certificate_operation(a).status.lower() == "inprogress"
    assert certs.get_certificate_operation(b).status.lower() == "completed"
