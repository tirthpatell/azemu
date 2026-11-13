package keyvault

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/sha512"
	"math/big"
	"net/http"
	"testing"

	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azkeys"
)

func createRSA(t *testing.T, c *azkeys.Client, name string, bits int32) azkeys.CreateKeyResponse {
	t.Helper()
	r, err := c.CreateKey(context.Background(), name, azkeys.CreateKeyParameters{Kty: ptr(azkeys.KeyTypeRSA), KeySize: ptr(bits)}, nil)
	if err != nil {
		t.Fatal(err)
	}
	return r
}

func rsaPub(k *azkeys.JSONWebKey) *rsa.PublicKey {
	return &rsa.PublicKey{N: new(big.Int).SetBytes(k.N), E: int(new(big.Int).SetBytes(k.E).Int64())}
}

func TestKey_CreateRSA2048ReturnsPublicPartsAndNoPrivateParts(t *testing.T) {
	c := keysClient(t, newVault())
	r := createRSA(t, c, "rsa1", 2048)
	k := r.Key
	if *k.Kty != azkeys.KeyTypeRSA || len(k.N) != 256 || len(k.E) == 0 {
		t.Fatalf("jwk: kty=%v n=%d", *k.Kty, len(k.N))
	}
	if len(k.D) != 0 || len(k.P) != 0 {
		t.Fatal("private parts must not be returned")
	}
	if k.KID.Name() != "rsa1" {
		t.Fatalf("kid %v", k.KID)
	}
}

func TestKey_SignRS256VerifiesWithDownloadedPublicKeyAndInVault(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	r := createRSA(t, c, "sign", 2048)
	digest := sha256.Sum256([]byte("hello"))
	sig, err := c.Sign(ctx, "sign", "", azkeys.SignParameters{Algorithm: ptr(azkeys.SignatureAlgorithmRS256), Value: digest[:]}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := rsa.VerifyPKCS1v15(rsaPub(r.Key), crypto.SHA256, digest[:], sig.Result); err != nil {
		t.Fatalf("local verify: %v", err)
	}
	v, err := c.Verify(ctx, "sign", "", azkeys.VerifyParameters{Algorithm: ptr(azkeys.SignatureAlgorithmRS256), Digest: digest[:], Signature: sig.Result}, nil)
	if err != nil || !*v.Value {
		t.Fatalf("verify in vault: %v %v", v.Value, err)
	}
	other := sha256.Sum256([]byte("other"))
	v, err = c.Verify(ctx, "sign", "", azkeys.VerifyParameters{Algorithm: ptr(azkeys.SignatureAlgorithmRS256), Digest: other[:], Signature: sig.Result}, nil)
	if err != nil || *v.Value {
		t.Fatalf("verify of wrong digest should be false: %v %v", v.Value, err)
	}
}

func TestKey_SignPS256VerifiesLocally(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	r := createRSA(t, c, "pss", 2048)
	digest := sha256.Sum256([]byte("pss"))
	sig, err := c.Sign(ctx, "pss", "", azkeys.SignParameters{Algorithm: ptr(azkeys.SignatureAlgorithmPS256), Value: digest[:]}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := rsa.VerifyPSS(rsaPub(r.Key), crypto.SHA256, digest[:], sig.Result, &rsa.PSSOptions{SaltLength: rsa.PSSSaltLengthEqualsHash}); err != nil {
		t.Fatalf("PSS verify: %v", err)
	}
}

func TestKey_ECSignAndVerifyES256AndES384WithLocalVerification(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	for _, tc := range []struct {
		name  string
		curve azkeys.CurveName
		alg   azkeys.SignatureAlgorithm
		ec    elliptic.Curve
		sum   func([]byte) []byte
	}{
		{"p256", azkeys.CurveNameP256, azkeys.SignatureAlgorithmES256, elliptic.P256(), func(b []byte) []byte { h := sha256.Sum256(b); return h[:] }},
		{"p384", azkeys.CurveNameP384, azkeys.SignatureAlgorithmES384, elliptic.P384(), func(b []byte) []byte { h := sha512.Sum384(b); return h[:] }},
	} {
		r, err := c.CreateKey(ctx, "ec"+tc.name, azkeys.CreateKeyParameters{Kty: ptr(azkeys.KeyTypeEC), Curve: ptr(tc.curve)}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if *r.Key.Crv != tc.curve {
			t.Fatalf("curve %v", *r.Key.Crv)
		}
		d := tc.sum([]byte("msg"))
		sig, err := c.Sign(ctx, "ec"+tc.name, "", azkeys.SignParameters{Algorithm: &tc.alg, Value: d}, nil)
		if err != nil {
			t.Fatal(err)
		}
		half := len(sig.Result) / 2
		pub := ecdsa.PublicKey{Curve: tc.ec, X: new(big.Int).SetBytes(r.Key.X), Y: new(big.Int).SetBytes(r.Key.Y)}
		if !ecdsa.Verify(&pub, d, new(big.Int).SetBytes(sig.Result[:half]), new(big.Int).SetBytes(sig.Result[half:])) {
			t.Fatalf("%s: signature does not verify (len %d)", tc.name, len(sig.Result))
		}
	}
}

func TestKey_EncryptThenDecryptRSAOAEP256RoundTripAndLocalDecrypt(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	r := createRSA(t, c, "enc", 2048)
	alg := azkeys.EncryptionAlgorithmRSAOAEP256
	enc, err := c.Encrypt(ctx, "enc", "", azkeys.KeyOperationParameters{Algorithm: &alg, Value: []byte("secret data")}, nil)
	if err != nil {
		t.Fatal(err)
	}
	dec, err := c.Decrypt(ctx, "enc", "", azkeys.KeyOperationParameters{Algorithm: &alg, Value: enc.Result}, nil)
	if err != nil || string(dec.Result) != "secret data" {
		t.Fatalf("decrypt: %q %v", dec.Result, err)
	}
	// Encrypt locally with the public key; the vault must decrypt it.
	local, err := rsa.EncryptOAEP(sha256.New(), rand.Reader, rsaPub(r.Key), []byte("local"), nil)
	if err != nil {
		t.Fatal(err)
	}
	dec, err = c.Decrypt(ctx, "enc", "", azkeys.KeyOperationParameters{Algorithm: &alg, Value: local}, nil)
	if err != nil || string(dec.Result) != "local" {
		t.Fatalf("vault decrypt of local ciphertext: %q %v", dec.Result, err)
	}
}

func TestKey_WrapThenUnwrapRSAOAEPReturnsTheSameKeyMaterial(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	createRSA(t, c, "wrap", 2048)
	alg := azkeys.EncryptionAlgorithmRSAOAEP
	cek := make([]byte, 32)
	rand.Read(cek)
	w, err := c.WrapKey(ctx, "wrap", "", azkeys.KeyOperationParameters{Algorithm: &alg, Value: cek}, nil)
	if err != nil {
		t.Fatal(err)
	}
	u, err := c.UnwrapKey(ctx, "wrap", "", azkeys.KeyOperationParameters{Algorithm: &alg, Value: w.Result}, nil)
	if err != nil || string(u.Result) != string(cek) {
		t.Fatalf("unwrap mismatch: %v", err)
	}
}

func TestKey_RotateMakesANewVersionAndKeepsTheName(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	first := createRSA(t, c, "rot", 2048)
	rot, err := c.RotateKey(ctx, "rot", nil)
	if err != nil {
		t.Fatal(err)
	}
	if rot.Key.KID.Name() != "rot" || rot.Key.KID.Version() == first.Key.KID.Version() {
		t.Fatalf("rotate: %v vs %v", rot.Key.KID, first.Key.KID)
	}
	latest, _ := c.GetKey(ctx, "rot", "", nil)
	if latest.Key.KID.Version() != rot.Key.KID.Version() {
		t.Fatal("latest is not the rotated version")
	}
	n := 0
	for p := c.NewListKeyPropertiesVersionsPager("rot", nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Value)
	}
	if n != 2 {
		t.Fatalf("versions %d", n)
	}
}

func TestKey_RotationPolicySetThenGet(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	createRSA(t, c, "pol", 2048)
	_, err := c.UpdateKeyRotationPolicy(ctx, "pol", azkeys.KeyRotationPolicy{
		Attributes: &azkeys.KeyRotationPolicyAttributes{ExpiryTime: ptr("P90D")},
		LifetimeActions: []*azkeys.LifetimeAction{{
			Action:  &azkeys.LifetimeActionType{Type: ptr(azkeys.KeyRotationPolicyActionRotate)},
			Trigger: &azkeys.LifetimeActionTrigger{TimeAfterCreate: ptr("P30D")},
		}},
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	got, err := c.GetKeyRotationPolicy(ctx, "pol", nil)
	if err != nil {
		t.Fatal(err)
	}
	// Azure adds a "notify" action (30 days before expiry) when an expiry time is set, so look for the rotate action.
	if *got.Attributes.ExpiryTime != "P90D" {
		t.Fatalf("expiry %v", *got.Attributes.ExpiryTime)
	}
	found := false
	for _, a := range got.LifetimeActions {
		if *a.Action.Type == azkeys.KeyRotationPolicyActionRotate && *a.Trigger.TimeAfterCreate == "P30D" {
			found = true
		}
	}
	if !found {
		t.Fatalf("rotate action missing: %d actions", len(got.LifetimeActions))
	}
}

func TestKey_ImportRSAKeyThenSignWithItsPrivatePart(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	priv, _ := rsa.GenerateKey(rand.Reader, 2048)
	priv.Precompute()
	jwk := &azkeys.JSONWebKey{
		Kty: ptr(azkeys.KeyTypeRSA), N: priv.N.Bytes(), E: big.NewInt(int64(priv.E)).Bytes(), D: priv.D.Bytes(),
		P: priv.Primes[0].Bytes(), Q: priv.Primes[1].Bytes(), DP: priv.Precomputed.Dp.Bytes(), DQ: priv.Precomputed.Dq.Bytes(), QI: priv.Precomputed.Qinv.Bytes(),
	}
	imp, err := c.ImportKey(ctx, "imp", azkeys.ImportKeyParameters{Key: jwk}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(imp.Key.D) != 0 {
		t.Fatal("import answered with private exponent")
	}
	d := sha256.Sum256([]byte("x"))
	sig, err := c.Sign(ctx, "imp", "", azkeys.SignParameters{Algorithm: ptr(azkeys.SignatureAlgorithmRS256), Value: d[:]}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := rsa.VerifyPKCS1v15(&priv.PublicKey, crypto.SHA256, d[:], sig.Result); err != nil {
		t.Fatalf("signature by imported key: %v", err)
	}
}

func TestKey_KeyOpsLimitRestrictsOperations(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	_, err := c.CreateKey(ctx, "ops", azkeys.CreateKeyParameters{Kty: ptr(azkeys.KeyTypeRSA), KeySize: ptr(int32(2048)),
		KeyOps: []*azkeys.KeyOperation{ptr(azkeys.KeyOperationEncrypt)}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	d := sha256.Sum256([]byte("x"))
	_, err = c.Sign(ctx, "ops", "", azkeys.SignParameters{Algorithm: ptr(azkeys.SignatureAlgorithmRS256), Value: d[:]}, nil)
	respErrInner(t, err, http.StatusForbidden, "Forbidden", "KeyOperationForbidden")
}

func TestKey_GetRandomBytesReturnsRequestedCount(t *testing.T) {
	c := keysClient(t, newVault())
	r, err := c.GetRandomBytes(context.Background(), azkeys.GetRandomBytesParameters{Count: ptr(int32(48))}, nil)
	if err != nil || len(r.Value) != 48 {
		t.Fatalf("%d %v", len(r.Value), err)
	}
}

func TestKey_DeleteRecoverAndGetMissingIsKeyNotFound(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	createRSA(t, c, "dk", 2048)
	if _, err := c.DeleteKey(ctx, "dk", nil); err != nil {
		t.Fatal(err)
	}
	_, err := c.GetKey(ctx, "dk", "", nil)
	respErr(t, err, http.StatusNotFound, "KeyNotFound")
	if _, err := c.RecoverDeletedKey(ctx, "dk", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := c.GetKey(ctx, "dk", "", nil); err != nil {
		t.Fatal(err)
	}
}

func TestKey_BackupRestoreKeepsPrivateKeyUsable(t *testing.T) {
	ctx := context.Background()
	a := keysClient(t, newVault())
	b := keysClient(t, newVault())
	r := createRSA(t, a, "bk", 2048)
	bk, err := a.BackupKey(ctx, "bk", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := b.RestoreKey(ctx, azkeys.RestoreKeyParameters{KeyBackup: bk.Value}, nil); err != nil {
		t.Fatal(err)
	}
	d := sha256.Sum256([]byte("m"))
	sig, err := b.Sign(ctx, "bk", "", azkeys.SignParameters{Algorithm: ptr(azkeys.SignatureAlgorithmRS256), Value: d[:]}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := rsa.VerifyPKCS1v15(rsaPub(r.Key), crypto.SHA256, d[:], sig.Result); err != nil {
		t.Fatalf("restored key signs differently: %v", err)
	}
}

func TestKey_ListPagesKeys(t *testing.T) {
	ctx := context.Background()
	c := keysClient(t, newVault())
	for i := 0; i < 3; i++ {
		createRSA(t, c, "lk"+string(rune('a'+i)), 2048)
	}
	n := 0
	for p := c.NewListKeyPropertiesPager(nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Value)
	}
	if n != 3 {
		t.Fatalf("n=%d", n)
	}
}
