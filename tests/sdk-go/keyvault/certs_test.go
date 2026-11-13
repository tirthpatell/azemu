package keyvault

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/pem"
	"math/big"
	"net/http"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azcertificates"
	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
)

func selfSignedPolicy(subject string) *azcertificates.CertificatePolicy {
	return &azcertificates.CertificatePolicy{
		IssuerParameters: &azcertificates.IssuerParameters{Name: ptr("Self")},
		KeyProperties: &azcertificates.KeyProperties{
			KeyType: ptr(azcertificates.KeyTypeRSA), KeySize: ptr(int32(2048)), Exportable: ptr(true), ReuseKey: ptr(true),
		},
		SecretProperties: &azcertificates.SecretProperties{ContentType: ptr("application/x-pkcs12")},
		X509CertificateProperties: &azcertificates.X509CertificateProperties{
			Subject: ptr(subject), ValidityInMonths: ptr(int32(12)),
			SubjectAlternativeNames: &azcertificates.SubjectAlternativeNames{DNSNames: []*string{ptr("example.test")}},
		},
	}
}

// waitCompleted polls the certificate operation the way a poller would.
func waitCompleted(t *testing.T, c *azcertificates.Client, name string) azcertificates.GetCertificateOperationResponse {
	t.Helper()
	deadline := time.Now().Add(30 * time.Second)
	for {
		op, err := c.GetCertificateOperation(context.Background(), name, nil)
		if err != nil {
			t.Fatal(err)
		}
		if op.Status != nil && *op.Status == "completed" {
			return op
		}
		if time.Now().After(deadline) {
			t.Fatalf("operation still %v", op.Status)
		}
		time.Sleep(200 * time.Millisecond)
	}
}

func TestCertificate_CreateSelfSignedAnswers202InProgressThenOperationCompletes(t *testing.T) {
	ctx := context.Background()
	c := certsClient(t, newVault())
	r, err := c.CreateCertificate(ctx, "ss", azcertificates.CreateCertificateParameters{CertificatePolicy: selfSignedPolicy("CN=azemu.test")}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if r.Status == nil || (*r.Status != "inProgress" && *r.Status != "completed") {
		t.Fatalf("create status %v", r.Status)
	}
	op := waitCompleted(t, c, "ss")
	if op.Target == nil {
		t.Fatal("completed operation has no target")
	}
	cert, err := c.GetCertificate(ctx, "ss", "", nil)
	if err != nil {
		t.Fatal(err)
	}
	x, err := x509.ParseCertificate(cert.CER)
	if err != nil {
		t.Fatalf("cer does not parse: %v", err)
	}
	if x.Subject.CommonName != "azemu.test" {
		t.Fatalf("CN %q", x.Subject.CommonName)
	}
	if len(x.DNSNames) != 1 || x.DNSNames[0] != "example.test" {
		t.Fatalf("SAN %v", x.DNSNames)
	}
	if x.Issuer.String() != x.Subject.String() {
		t.Fatal("self-signed issuer must equal subject")
	}
	months := x.NotAfter.Sub(x.NotBefore).Hours() / 24 / 30.4
	if months < 11 || months > 13 {
		t.Fatalf("validity %.1f months", months)
	}
	if cert.X509Thumbprint == nil || len(cert.X509Thumbprint) != 20 {
		t.Fatalf("x5t len %d", len(cert.X509Thumbprint))
	}
}

func TestCertificate_SecretBehindTheCertificateHoldsAPrivateKeyBundle(t *testing.T) {
	ctx := context.Background()
	v := newVault()
	c := certsClient(t, v)
	c.CreateCertificate(ctx, "withkey", azcertificates.CreateCertificateParameters{
		CertificatePolicy: func() *azcertificates.CertificatePolicy {
			p := selfSignedPolicy("CN=pem.test")
			p.SecretProperties.ContentType = ptr("application/x-pem-file")
			return p
		}()}, nil)
	waitCompleted(t, c, "withkey")
	s := secretsClient(t, v)
	sec, err := s.GetSecret(ctx, "withkey", "", nil)
	if err != nil {
		t.Fatal(err)
	}
	blockTypes := map[string]bool{}
	rest := []byte(*sec.Value)
	for {
		var b *pem.Block
		b, rest = pem.Decode(rest)
		if b == nil {
			break
		}
		blockTypes[b.Type] = true
	}
	if !blockTypes["CERTIFICATE"] || !(blockTypes["PRIVATE KEY"] || blockTypes["RSA PRIVATE KEY"]) {
		t.Fatalf("PEM blocks: %v", blockTypes)
	}
	if sec.ContentType == nil || *sec.ContentType != "application/x-pem-file" {
		t.Fatalf("content type %v", sec.ContentType)
	}
	_ = azsecrets.SetSecretParameters{}
}

func TestCertificate_PolicyIsReturnedAsCreated(t *testing.T) {
	ctx := context.Background()
	c := certsClient(t, newVault())
	c.CreateCertificate(ctx, "pol", azcertificates.CreateCertificateParameters{CertificatePolicy: selfSignedPolicy("CN=pol.test")}, nil)
	waitCompleted(t, c, "pol")
	p, err := c.GetCertificatePolicy(ctx, "pol", nil)
	if err != nil {
		t.Fatal(err)
	}
	if *p.IssuerParameters.Name != "Self" || *p.KeyProperties.KeySize != 2048 || *p.KeyProperties.KeyType != azcertificates.KeyTypeRSA {
		t.Fatalf("policy %+v %+v", p.IssuerParameters, p.KeyProperties)
	}
	if *p.X509CertificateProperties.Subject != "CN=pol.test" || *p.X509CertificateProperties.ValidityInMonths != 12 {
		t.Fatalf("x509 %+v", p.X509CertificateProperties)
	}
}

func TestCertificate_ECP256SelfSignedUsesAnECDSAKey(t *testing.T) {
	ctx := context.Background()
	c := certsClient(t, newVault())
	pol := selfSignedPolicy("CN=ec.test")
	pol.KeyProperties = &azcertificates.KeyProperties{KeyType: ptr(azcertificates.KeyTypeEC), Curve: ptr(azcertificates.CurveNameP256)}
	if _, err := c.CreateCertificate(ctx, "ec", azcertificates.CreateCertificateParameters{CertificatePolicy: pol}, nil); err != nil {
		t.Fatal(err)
	}
	waitCompleted(t, c, "ec")
	cert, _ := c.GetCertificate(ctx, "ec", "", nil)
	x, err := x509.ParseCertificate(cert.CER)
	if err != nil {
		t.Fatal(err)
	}
	pk, ok := x.PublicKey.(*ecdsa.PublicKey)
	if !ok || pk.Curve != elliptic.P256() {
		t.Fatalf("public key %T", x.PublicKey)
	}
}

func TestCertificate_ImportPEMThenReadBackItsSubjectAndKeyAndPolicyFromTheCert(t *testing.T) {
	ctx := context.Background()
	c := certsClient(t, newVault())
	key, _ := rsa.GenerateKey(rand.Reader, 2048)
	tmpl := &x509.Certificate{SerialNumber: big.NewInt(42), Subject: pkix.Name{CommonName: "imported.test"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(24 * 365 * time.Hour), DNSNames: []string{"imported.test"}}
	der, _ := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	keyDer, _ := x509.MarshalPKCS8PrivateKey(key)
	pemBytes := append(pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDer}), pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})...)
	s := base64.StdEncoding.EncodeToString(pemBytes) // the value is always base64, even for PEM bytes (as in the Python and .NET SDKs)
	r, err := c.ImportCertificate(ctx, "imp", azcertificates.ImportCertificateParameters{
		Base64EncodedCertificate: &s,
		CertificatePolicy:        &azcertificates.CertificatePolicy{SecretProperties: &azcertificates.SecretProperties{ContentType: ptr("application/x-pem-file")}},
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if string(r.CER) != string(der) {
		t.Fatal("cer differs from the imported certificate")
	}
	p, err := c.GetCertificatePolicy(ctx, "imp", nil)
	if err != nil {
		t.Fatal(err)
	}
	if p.X509CertificateProperties == nil || *p.X509CertificateProperties.Subject != "CN=imported.test" {
		t.Fatalf("policy subject %+v", p.X509CertificateProperties)
	}
}

func TestCertificate_ListGetVersionsAndUpdateTags(t *testing.T) {
	ctx := context.Background()
	c := certsClient(t, newVault())
	for i := 0; i < 2; i++ {
		c.CreateCertificate(ctx, "lv", azcertificates.CreateCertificateParameters{CertificatePolicy: selfSignedPolicy("CN=lv.test")}, nil)
		waitCompleted(t, c, "lv")
	}
	n := 0
	for p := c.NewListCertificatePropertiesVersionsPager("lv", nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Value)
	}
	if n != 2 {
		t.Fatalf("versions %d", n)
	}
	up, err := c.UpdateCertificate(ctx, "lv", "", azcertificates.UpdateCertificateParameters{Tags: map[string]*string{"a": ptr("b")}}, nil)
	if err != nil || *up.Tags["a"] != "b" {
		t.Fatalf("update: %v", err)
	}
	cnt := 0
	for p := c.NewListCertificatePropertiesPager(nil); p.More(); {
		pg, _ := p.NextPage(ctx)
		cnt += len(pg.Value)
	}
	if cnt != 1 {
		t.Fatalf("list %d", cnt)
	}
}

func TestCertificate_DeleteMovesLinkedKeyAndSecretAndRecoverBringsThemBack(t *testing.T) {
	ctx := context.Background()
	v := newVault()
	c := certsClient(t, v)
	c.CreateCertificate(ctx, "dc", azcertificates.CreateCertificateParameters{CertificatePolicy: selfSignedPolicy("CN=dc.test")}, nil)
	waitCompleted(t, c, "dc")
	if _, err := c.DeleteCertificate(ctx, "dc", nil); err != nil {
		t.Fatal(err)
	}
	_, err := c.GetCertificate(ctx, "dc", "", nil)
	respErr(t, err, http.StatusNotFound, "CertificateNotFound")
	_, err = secretsClient(t, v).GetSecret(ctx, "dc", "", nil)
	respErr(t, err, http.StatusNotFound, "SecretNotFound")
	if _, err := c.RecoverDeletedCertificate(ctx, "dc", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := secretsClient(t, v).GetSecret(ctx, "dc", "", nil); err != nil {
		t.Fatalf("secret after recover: %v", err)
	}
}

func TestCertificate_IssuerAndContactsRoundTrip(t *testing.T) {
	ctx := context.Background()
	c := certsClient(t, newVault())
	if _, err := c.SetIssuer(ctx, "myissuer", azcertificates.SetIssuerParameters{Provider: ptr("Test")}, nil); err != nil {
		t.Fatal(err)
	}
	got, err := c.GetIssuer(ctx, "myissuer", nil)
	if err != nil || *got.Provider != "Test" {
		t.Fatalf("issuer %v %v", got.Provider, err)
	}
	_, err = c.SetContacts(ctx, azcertificates.Contacts{ContactList: []*azcertificates.Contact{{Email: ptr("a@example.test"), Name: ptr("A")}}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	ct, err := c.GetContacts(ctx, nil)
	if err != nil || len(ct.ContactList) != 1 || *ct.ContactList[0].Email != "a@example.test" {
		t.Fatalf("contacts %v", err)
	}
}

func TestCertificate_BackupRestoreRoundTrip(t *testing.T) {
	ctx := context.Background()
	a := certsClient(t, newVault())
	b := certsClient(t, newVault())
	a.CreateCertificate(ctx, "bk", azcertificates.CreateCertificateParameters{CertificatePolicy: selfSignedPolicy("CN=bk.test")}, nil)
	waitCompleted(t, a, "bk")
	bk, err := a.BackupCertificate(ctx, "bk", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := b.RestoreCertificate(ctx, azcertificates.RestoreCertificateParameters{CertificateBackup: bk.Value}, nil); err != nil {
		t.Fatal(err)
	}
	ca, _ := a.GetCertificate(ctx, "bk", "", nil)
	cb, err := b.GetCertificate(ctx, "bk", "", nil)
	if err != nil || string(ca.CER) != string(cb.CER) {
		t.Fatalf("restore: %v", err)
	}
}

func TestCertificate_BadSubjectIsBadParameter400(t *testing.T) {
	c := certsClient(t, newVault())
	_, err := c.CreateCertificate(context.Background(), "bad", azcertificates.CreateCertificateParameters{CertificatePolicy: selfSignedPolicy("not a dn")}, nil)
	respErr(t, err, http.StatusBadRequest, "BadParameter")
}
