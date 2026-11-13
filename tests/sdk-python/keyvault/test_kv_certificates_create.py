"""Certificates: begin_create_certificate and what the created certificate contains.

Sources:
  * Recorded (azure-keyvault-certificates recordings, Microsoft's own tests): create with
    the default policy (POST /certificates/{n}/create -> 202 with a pending operation whose
    status is "inProgress" and which later reads "completed", target = certificate URL
    without a version), the default policy values Key Vault fills in (RSA 2048, not reusable
    key, PKCS12, 12 months, digitalSignature + keyEncipherment, serverAuth + clientAuth EKUs,
    CA false, AutoRenew at 80 %), RSA 2048 only, SAN DNS names / UPNs / IP addresses, a
    SAN-only policy without subject, validity 12 and 24 months, Self and Unknown issuers,
    preserveCertOrder, lifetime action EmailContacts at 98 %. In the recorded certificates:
    notBefore = created - 10 minutes, notAfter = created + the policy's months, the
    extensions are keyUsage (critical), basicConstraints, extendedKeyUsage, subjectAltName,
    authorityKeyIdentifier == subjectKeyIdentifier (self-signed), x5t = base64url(SHA-1(DER)),
    UPN SANs are otherName 1.3.6.1.4.1.311.20.2.3 with a UTF8String.
  * "From Azure docs" (no recording): RSA 3072 / 4096, EC curves, e-mail and URI SANs, custom
    key usage and EKU values, create with enabled=False / tags, validity values other than
    12 and 24. These are marked in the tests.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
from azure.keyvault.certificates import (
    CertificateContentType,
    CertificateOperation,
    CertificatePolicy,
    CertificatePolicyAction,
    KeyCurveName,
    KeyType,
    KeyUsageType,
    KeyVaultCertificate,
    LifetimeAction,
)
from cryptography import x509
from cryptography.hazmat.primitives.asymmetric import ec, rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

from kvutil import (
    POLL,
    add_months,
    begin_pending,
    create_cert,
    ec_policy,
    sha1_of,
    subject_cn,
    san_upns,
    san_values,
    unique,
    unknown_policy,
    x509_of,
)

HEX32 = re.compile(r"^[0-9a-f]{32}$")
SERVER_AUTH = ExtendedKeyUsageOID.SERVER_AUTH
CLIENT_AUTH = ExtendedKeyUsageOID.CLIENT_AUTH


def _utcnow():
    return datetime.now(timezone.utc)


# -- the default policy -------------------------------------------------------
def test_create_default_policy_returns_certificate(certs):
    name = unique("cert")
    cert = create_cert(certs, name)
    assert isinstance(cert, KeyVaultCertificate)
    assert cert.name == name
    assert cert.cer
    assert cert.policy is not None


def test_create_poller_status_is_finished_after_wait(certs):
    name = unique("cert")
    poller = certs.begin_create_certificate(name, CertificatePolicy.get_default(), _polling_interval=POLL)
    poller.wait()
    assert poller.done()
    # The create poller reports the status of the certificate operation, which is "completed".
    assert poller.status().lower() == "completed"


def test_create_ids_follow_vault_url_name_version(certs):
    name = unique("cert")
    cert = create_cert(certs, name)
    version = cert.properties.version
    assert HEX32.match(version), version
    assert cert.id == f"{certs.vault_url}/certificates/{name}/{version}"
    assert cert.key_id == f"{certs.vault_url}/keys/{name}/{version}"
    assert cert.secret_id == f"{certs.vault_url}/secrets/{name}/{version}"
    assert cert.properties.id == cert.id
    assert cert.properties.name == name
    assert cert.properties.vault_url == certs.vault_url


def test_create_default_policy_values_filled_in_by_the_service(certs):
    # Recorded: the default policy sends only subject and issuer; Key Vault fills in the rest.
    name = unique("cert")
    cert = create_cert(certs, name)
    p = cert.policy
    assert p.issuer_name == "Self"
    assert p.subject == "CN=DefaultPolicy"
    assert p.exportable is True
    assert p.key_type == KeyType.rsa
    assert p.key_size == 2048
    assert p.reuse_key is False
    assert p.content_type == CertificateContentType.pkcs12
    assert p.validity_in_months == 12
    assert set(p.key_usage) == {KeyUsageType.digital_signature, KeyUsageType.key_encipherment}
    assert set(p.enhanced_key_usage) == {"1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"}
    assert len(p.lifetime_actions) == 1
    assert p.lifetime_actions[0].action == CertificatePolicyAction.auto_renew
    assert p.lifetime_actions[0].lifetime_percentage == 80
    assert p.lifetime_actions[0].days_before_expiry is None


def test_create_get_certificate_returns_the_same_policy_and_version(certs):
    name = unique("cert")
    created = create_cert(certs, name)
    got = certs.get_certificate(name)
    assert got.id == created.id
    assert got.cer == created.cer
    assert got.policy.subject == "CN=DefaultPolicy"
    assert got.policy.issuer_name == "Self"


# -- the operation ------------------------------------------------------------
def test_get_certificate_operation_after_create_is_completed(certs):
    name = unique("cert")
    cert = create_cert(certs, name)
    op = certs.get_certificate_operation(name)
    assert isinstance(op, CertificateOperation)
    assert op.status.lower() == "completed"
    assert op.issuer_name == "Self"
    assert op.cancellation_requested is False
    assert op.id == f"{certs.vault_url}/certificates/{name}/pending"
    assert op.target == f"{certs.vault_url}/certificates/{name}"
    assert op.request_id
    assert cert.name == name


def test_certificate_operation_csr_is_a_valid_request_for_the_policy_subject(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy("CN=csr-subject"))
    csr = x509.load_der_x509_csr(certs.get_certificate_operation(name).csr)
    assert csr.is_signature_valid
    assert csr.subject.get_attributes_for_oid(NameOID.COMMON_NAME)[0].value == "csr-subject"


def test_certificate_operation_csr_matches_the_certificate_public_key(certs):
    name = unique("cert")
    cert = create_cert(certs, name, ec_policy("CN=csr-key"))
    csr = x509.load_der_x509_csr(certs.get_certificate_operation(name).csr)
    assert csr.public_key().public_numbers() == x509_of(cert).public_key().public_numbers()


# -- the certificate itself ---------------------------------------------------
def test_cer_is_der_x509_with_policy_subject(certs):
    name = unique("cert")
    cert = create_cert(certs, name)
    c = x509_of(cert)
    assert c.subject.rfc4514_string() == "CN=DefaultPolicy"
    assert subject_cn(c) == "DefaultPolicy"


def test_cer_is_self_signed_and_signature_verifies(certs):
    name = unique("cert")
    c = x509_of(create_cert(certs, name))
    assert c.issuer == c.subject
    c.verify_directly_issued_by(c)  # raises when the signature does not verify with its own key


def test_cer_rsa_self_signature_is_sha256_with_rsa(certs):
    # Recorded: sha256WithRSAEncryption.
    c = x509_of(create_cert(certs, unique("cert")))
    assert c.signature_algorithm_oid.dotted_string == "1.2.840.113549.1.1.11"


def test_cer_default_extensions(certs):
    # Recorded extensions of a default-policy certificate.
    c = x509_of(create_cert(certs, unique("cert")))
    ku = c.extensions.get_extension_for_class(x509.KeyUsage)
    assert ku.critical is True
    assert ku.value.digital_signature is True
    assert ku.value.key_encipherment is True
    assert ku.value.key_cert_sign is False
    assert ku.value.data_encipherment is False
    eku = c.extensions.get_extension_for_class(x509.ExtendedKeyUsage).value
    assert set(eku) == {SERVER_AUTH, CLIENT_AUTH}
    bc = c.extensions.get_extension_for_class(x509.BasicConstraints).value
    assert bc.ca is False


def test_cer_basic_constraints_is_not_critical(certs):
    # Recorded: basicConstraints (CA false) is not marked critical in the certificates Key Vault issues.
    c = x509_of(create_cert(certs, unique("cert")))
    assert c.extensions.get_extension_for_class(x509.BasicConstraints).critical is False


def test_cer_subject_key_identifier_equals_authority_key_identifier(certs):
    # Recorded: self-signed certificates carry SKI == AKI.
    c = x509_of(create_cert(certs, unique("cert")))
    ski = c.extensions.get_extension_for_class(x509.SubjectKeyIdentifier).value
    aki = c.extensions.get_extension_for_class(x509.AuthorityKeyIdentifier).value
    assert aki.key_identifier == ski.digest


def test_cer_is_rsa_2048_for_default_policy(certs):
    c = x509_of(create_cert(certs, unique("cert")))
    key = c.public_key()
    assert isinstance(key, rsa.RSAPublicKey)
    assert key.key_size == 2048
    assert key.public_numbers().e == 65537


def test_x509_thumbprint_is_sha1_of_the_der(certs):
    cert = create_cert(certs, unique("cert"))
    assert cert.properties.x509_thumbprint == sha1_of(cert.cer)
    assert len(cert.properties.x509_thumbprint) == 20


def test_properties_dates_match_the_certificate_and_the_policy(certs):
    # Recorded: attributes.nbf / exp are the certificate's notBefore / notAfter; created is
    # about ten minutes after nbf; exp is created + 12 calendar months.
    cert = create_cert(certs, unique("cert"))
    c = x509_of(cert)
    p = cert.properties
    assert p.not_before == c.not_valid_before_utc.replace(microsecond=0)
    assert p.expires_on == c.not_valid_after_utc.replace(microsecond=0)
    assert abs(p.created_on - _utcnow()) < timedelta(minutes=2)
    assert abs(p.updated_on - p.created_on) < timedelta(seconds=10)
    assert abs((p.created_on - p.not_before) - timedelta(minutes=10)) < timedelta(minutes=1)
    assert abs(p.expires_on - add_months(p.created_on, 12)) < timedelta(days=1)


def test_properties_enabled_recovery_level_and_no_tags_by_default(certs):
    cert = create_cert(certs, unique("cert"))
    p = cert.properties
    assert p.enabled is True
    assert p.tags is None
    assert p.recovery_level in {
        "Purgeable",
        "Recoverable+Purgeable",
        "Recoverable",
        "Recoverable+ProtectedSubscription",
        "CustomizedRecoverable+Purgeable",
        "CustomizedRecoverable",
        "CustomizedRecoverable+ProtectedSubscription",
    }


@pytest.mark.parametrize("months", [12, 24])
def test_validity_in_months_recorded_values(certs, months):
    cert = create_cert(certs, unique("cert"), ec_policy(validity_in_months=months))
    assert cert.policy.validity_in_months == months
    p = cert.properties
    assert abs(p.expires_on - add_months(p.created_on, months)) < timedelta(days=1)


@pytest.mark.parametrize("months", [1, 6, 36])
def test_validity_in_months_other_values(certs, months):
    # From Azure docs (validity_months is any positive number of months); only 12 and 24 are recorded.
    cert = create_cert(certs, unique("cert"), ec_policy(validity_in_months=months))
    assert cert.policy.validity_in_months == months
    c = x509_of(cert)
    assert abs(c.not_valid_after_utc - add_months(c.not_valid_before_utc + timedelta(minutes=10), months)) < timedelta(days=2)


# -- keys ---------------------------------------------------------------------
@pytest.mark.parametrize("size", [2048, 3072, 4096])
def test_rsa_key_sizes(certs, size):
    # RSA sizes 2048 / 3072 / 4096 are in the spec; only 2048 is recorded.
    name = unique("rsa")
    policy = CertificatePolicy(
        "Self", subject="CN=rsa-size", key_type=KeyType.rsa, key_size=size, exportable=True, reuse_key=False
    )
    cert = create_cert(certs, name, policy)
    assert cert.policy.key_type == KeyType.rsa
    assert cert.policy.key_size == size
    key = x509_of(cert).public_key()
    assert isinstance(key, rsa.RSAPublicKey)
    assert key.key_size == size


@pytest.mark.parametrize(
    "curve,expected",
    [
        (KeyCurveName.p_256, ec.SECP256R1),
        (KeyCurveName.p_384, ec.SECP384R1),
        (KeyCurveName.p_521, ec.SECP521R1),
    ],
)
def test_ec_curves(certs, curve, expected):
    # From Azure docs (policy key_props.crv: P-256, P-384, P-521); no EC create in the recordings.
    name = unique("ec")
    policy = CertificatePolicy("Self", subject="CN=ec-curve", key_type=KeyType.ec, key_curve_name=curve)
    cert = create_cert(certs, name, policy)
    assert cert.policy.key_type == KeyType.ec
    assert cert.policy.key_curve_name == curve
    key = x509_of(cert).public_key()
    assert isinstance(key, ec.EllipticCurvePublicKey)
    assert isinstance(key.curve, expected)
    x509_of(cert).verify_directly_issued_by(x509_of(cert))


@pytest.mark.parametrize("curve", ["P-256", "P-384"])
def test_ec_certificate_is_self_signed_and_signature_verifies(certs, curve):
    cert = create_cert(certs, unique("ec"), ec_policy("CN=ec-self", key_curve_name=curve))
    c = x509_of(cert)
    assert c.issuer == c.subject
    c.verify_directly_issued_by(c)
    assert c.signature_algorithm_oid.dotted_string.startswith("1.2.840.10045.4.3.")  # ecdsa-with-SHA*


def test_exportable_false_and_reuse_key_true_round_trip_in_policy(certs):
    cert = create_cert(certs, unique("cert"), ec_policy(exportable=False, reuse_key=True))
    assert cert.policy.exportable is False
    assert cert.policy.reuse_key is True


def test_content_type_pem_round_trips_in_policy(certs):
    policy = ec_policy(content_type=CertificateContentType.pem)
    cert = create_cert(certs, unique("cert"), policy)
    assert cert.policy.content_type == CertificateContentType.pem


# -- subject and subject alternative names -------------------------------------
def test_subject_with_several_attributes(certs):
    # From Azure docs: the subject is an X.500 distinguished name.
    cert = create_cert(certs, unique("cert"), ec_policy("CN=host.example.test, O=Azemu Org, C=US"))
    c = x509_of(cert)
    assert subject_cn(c) == "host.example.test"
    assert c.subject.get_attributes_for_oid(NameOID.ORGANIZATION_NAME)[0].value == "Azemu Org"
    assert c.subject.get_attributes_for_oid(NameOID.COUNTRY_NAME)[0].value == "US"
    assert cert.policy.subject == "CN=host.example.test, O=Azemu Org, C=US"


def test_san_dns_names(certs):
    # Recorded policy: CN=*.microsoft.com with san dns_names ["sdk.azure-int.net"].
    names = ["sdk.azure-int.net", "www.example.test"]
    cert = create_cert(certs, unique("cert"), ec_policy("CN=*.microsoft.com", san_dns_names=names))
    c = x509_of(cert)
    assert subject_cn(c) == "*.microsoft.com"
    assert set(san_values(c, x509.DNSName)) == set(names)
    assert set(cert.policy.san_dns_names) == set(names)


def test_san_emails(certs):
    # From Azure docs: policy x509_props.sans.emails.
    mails = ["a@example.test", "b@example.test"]
    cert = create_cert(certs, unique("cert"), ec_policy(san_emails=mails))
    assert set(san_values(x509_of(cert), x509.RFC822Name)) == set(mails)
    assert set(cert.policy.san_emails) == set(mails)


def test_san_user_principal_names(certs):
    # Recorded: UPNs become otherName 1.3.6.1.4.1.311.20.2.3 with a UTF8String.
    upns = ["john.doe@example.test"]
    cert = create_cert(certs, unique("cert"), ec_policy(san_user_principal_names=upns))
    assert san_upns(x509_of(cert)) == upns
    assert cert.policy.san_user_principal_names == upns


def test_san_ip_addresses_and_empty_subject(certs):
    # Recorded: a policy with only IP SANs (no subject) is accepted; the subject is empty.
    policy = CertificatePolicy("Self", san_ip_addresses=["10.0.0.1", "192.168.1.1"], content_type=CertificateContentType.pkcs12)
    cert = create_cert(certs, unique("cert"), policy)
    c = x509_of(cert)
    assert len(c.subject) == 0
    ips = {str(ip) for ip in san_values(c, x509.IPAddress)}
    assert ips == {"10.0.0.1", "192.168.1.1"}
    assert set(cert.policy.san_ip_addresses) == {"10.0.0.1", "192.168.1.1"}
    assert not cert.policy.san_dns_names


def test_san_uris(certs):
    # The policy is accepted (recorded); the URI in the certificate is from the X.509 SAN definition.
    uris = ["https://service.example.com/api"]
    policy = CertificatePolicy("Self", san_uris=uris, content_type=CertificateContentType.pkcs12)
    cert = create_cert(certs, unique("cert"), policy)
    assert san_values(x509_of(cert), x509.UniformResourceIdentifier) == uris
    assert cert.policy.san_uris == uris


# -- key usage, EKU, lifetime actions -------------------------------------------
def test_custom_key_usage(certs):
    # From Azure docs: key_usage values are the X.509 key usage names.
    usage = [KeyUsageType.digital_signature, KeyUsageType.non_repudiation]
    cert = create_cert(certs, unique("cert"), ec_policy(key_usage=usage))
    ku = x509_of(cert).extensions.get_extension_for_class(x509.KeyUsage).value
    assert ku.digital_signature is True
    assert ku.content_commitment is True
    assert ku.key_encipherment is False
    assert set(cert.policy.key_usage) == set(usage)


def test_custom_enhanced_key_usage(certs):
    # Recorded policy used the two default EKU OIDs; a different OID is from Azure docs.
    code_signing = "1.3.6.1.5.5.7.3.3"
    cert = create_cert(certs, unique("cert"), ec_policy(enhanced_key_usage=[code_signing]))
    eku = x509_of(cert).extensions.get_extension_for_class(x509.ExtendedKeyUsage).value
    assert [o.dotted_string for o in eku] == [code_signing]
    assert cert.policy.enhanced_key_usage == [code_signing]


def test_recorded_eku_pair_and_decipher_only_round_trip_in_policy(certs):
    # Recorded (test_policy): ekus serverAuth + clientAuth, key_usage ["decipherOnly"]; policy returned unchanged.
    policy = ec_policy(
        enhanced_key_usage=["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"],
        key_usage=[KeyUsageType.decipher_only],
        san_dns_names=["sdk.azure-int.net"],
    )
    name = unique("cert")
    certs.begin_create_certificate(name, policy, _polling_interval=POLL).wait()
    got = certs.get_certificate_policy(name)
    assert set(got.key_usage) == {KeyUsageType.decipher_only}
    assert set(got.enhanced_key_usage) == {"1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"}


def test_lifetime_action_email_contacts_at_percentage(certs):
    # Recorded: EmailContacts at 98 %.
    la = [LifetimeAction(action=CertificatePolicyAction.email_contacts, lifetime_percentage=98)]
    cert = create_cert(certs, unique("cert"), ec_policy(lifetime_actions=la))
    assert len(cert.policy.lifetime_actions) == 1
    got = cert.policy.lifetime_actions[0]
    assert got.action == CertificatePolicyAction.email_contacts
    assert got.lifetime_percentage == 98
    assert got.days_before_expiry is None


def test_lifetime_action_auto_renew_at_percentage(certs):
    # Recorded: AutoRenew at 80 % (test_crud_operations).
    la = [LifetimeAction(action=CertificatePolicyAction.auto_renew, lifetime_percentage=80)]
    cert = create_cert(certs, unique("cert"), ec_policy(lifetime_actions=la))
    got = cert.policy.lifetime_actions[0]
    assert got.action == CertificatePolicyAction.auto_renew
    assert got.lifetime_percentage == 80


def test_lifetime_action_days_before_expiry(certs):
    # From Azure docs: the trigger is either a percentage or a number of days before expiry.
    la = [LifetimeAction(action=CertificatePolicyAction.email_contacts, days_before_expiry=30)]
    cert = create_cert(certs, unique("cert"), ec_policy(lifetime_actions=la, validity_in_months=12))
    got = cert.policy.lifetime_actions[0]
    assert got.action == CertificatePolicyAction.email_contacts
    assert got.days_before_expiry == 30
    assert got.lifetime_percentage is None


def test_two_lifetime_actions(certs):
    # From Azure docs: a policy may hold one EmailContacts and one AutoRenew action.
    la = [
        LifetimeAction(action=CertificatePolicyAction.email_contacts, lifetime_percentage=60),
        LifetimeAction(action=CertificatePolicyAction.auto_renew, lifetime_percentage=90),
    ]
    cert = create_cert(certs, unique("cert"), ec_policy(lifetime_actions=la))
    by_action = {a.action: a for a in cert.policy.lifetime_actions}
    assert by_action[CertificatePolicyAction.email_contacts].lifetime_percentage == 60
    assert by_action[CertificatePolicyAction.auto_renew].lifetime_percentage == 90


def test_certificate_transparency_false_round_trips(certs):
    # Recorded in test_policy / test_merge_certificate: issuer.cert_transparency false.
    cert = create_cert(certs, unique("cert"), ec_policy(certificate_transparency=False))
    assert cert.policy.certificate_transparency is False


# -- create attributes ------------------------------------------------------------
def test_create_with_tags(certs):
    # From Azure docs: tags are arbitrary key/value metadata (not in a recorded create).
    cert = create_cert(certs, unique("cert"), ec_policy(), tags={"env": "test", "team": "azemu"})
    assert cert.properties.tags == {"env": "test", "team": "azemu"}
    assert certs.get_certificate(cert.name).properties.tags == {"env": "test", "team": "azemu"}


def test_create_disabled_certificate(certs):
    # From Azure docs: the enabled attribute of the create request.
    name = unique("cert")
    cert = create_cert(certs, name, ec_policy(), enabled=False)
    assert cert.properties.enabled is False
    assert certs.get_certificate(name).properties.enabled is False


def test_create_with_preserve_order(certs):
    # Recorded: preserveCertOrder true in the request is echoed in the certificate bundle.
    cert = create_cert(certs, unique("cert"), CertificatePolicy.get_default(), preserve_order=True)
    assert cert.properties.preserve_order is True


# -- unknown issuer: the create stays pending ---------------------------------------
def test_unknown_issuer_returns_in_progress_operation(certs):
    # Recorded (test_unknown_issuer_response): the poller result is the CertificateOperation,
    # status inProgress and status_details that tell the caller to merge.
    name = unique("ext")
    result = begin_pending(certs, name)
    assert isinstance(result, CertificateOperation)
    assert result.status.lower() == "inprogress"
    assert result.issuer_name == "Unknown"
    assert result.cancellation_requested is False
    assert result.id == f"{certs.vault_url}/certificates/{name}/pending"
    assert result.csr


def test_unknown_issuer_status_details_ask_for_a_merge(certs):
    # Recorded (test_unknown_issuer_response, test_merge_certificate):
    # "Pending certificate created. Please Perform Merge to complete the request."
    op = begin_pending(certs, unique("ext"))
    assert op.status_details == "Pending certificate created. Please Perform Merge to complete the request."


def test_unknown_issuer_csr_has_the_policy_subject_and_sans(certs):
    name = unique("ext")
    policy = unknown_policy("CN=ext.example.test", san_dns_names=["ext.example.test", "alt.example.test"])
    op = begin_pending(certs, name, policy)
    csr = x509.load_der_x509_csr(op.csr)
    assert csr.is_signature_valid
    assert subject_cn(csr) == "ext.example.test"
    sans = csr.extensions.get_extension_for_class(x509.SubjectAlternativeName).value
    assert set(sans.get_values_for_type(x509.DNSName)) == {"ext.example.test", "alt.example.test"}


def test_unknown_issuer_operation_stays_in_progress_when_read_again(certs):
    name = unique("ext")
    begin_pending(certs, name)
    op = certs.get_certificate_operation(name)
    assert op.status.lower() == "inprogress"
    assert op.target is None


def test_unknown_issuer_has_no_certificate_until_merged(certs):
    # A pending certificate has no certificate version yet: it reads as a bundle without `cer`
    # (the JavaScript SDK needs this; before it was a 404).
    name = unique("ext")
    begin_pending(certs, name)
    cert = certs.get_certificate(name)
    assert cert.cer is None
    assert cert.policy.issuer_name == "Unknown"


def test_unknown_issuer_policy_is_readable_while_pending(certs):
    name = unique("ext")
    begin_pending(certs, name, unknown_policy("CN=pending-policy"))
    policy = certs.get_certificate_policy(name)
    assert policy.issuer_name == "Unknown"
    assert policy.subject == "CN=pending-policy"


# -- client-side checks (no request is sent) ------------------------------------------
def test_policy_without_subject_or_sans_is_rejected_by_the_sdk(certs):
    # Recorded in Microsoft's test_policy_expected_errors_for_create_cert: raised before any request.
    with pytest.raises(ValueError):
        certs.begin_create_certificate(unique("cert"), CertificatePolicy("Self"))
    with pytest.raises(ValueError):
        certs.begin_create_certificate(unique("cert"), CertificatePolicy())
