package keyvault

import (
	"context"
	"fmt"
	"net/http"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/security/keyvault/azsecrets"
)

func TestSecret_SetThenGetLatestReturnsValueAndVersionedID(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	set, err := c.SetSecret(ctx, "db-password", azsecrets.SetSecretParameters{Value: ptr("hunter2")}, nil)
	if err != nil {
		t.Fatal(err)
	}
	got, err := c.GetSecret(ctx, "db-password", "", nil)
	if err != nil {
		t.Fatal(err)
	}
	if *got.Value != "hunter2" {
		t.Fatalf("value %q", *got.Value)
	}
	if got.ID.Name() != "db-password" || got.ID.Version() == "" || got.ID.Version() != set.ID.Version() {
		t.Fatalf("ids: %v vs %v", got.ID, set.ID)
	}
	if got.Attributes == nil || got.Attributes.Enabled == nil || !*got.Attributes.Enabled {
		t.Fatal("secret should be enabled")
	}
}

func TestSecret_SecondSetMakesNewVersionAndOldVersionStaysReadable(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	v1, _ := c.SetSecret(ctx, "rot", azsecrets.SetSecretParameters{Value: ptr("one")}, nil)
	v2, err := c.SetSecret(ctx, "rot", azsecrets.SetSecretParameters{Value: ptr("two")}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if v1.ID.Version() == v2.ID.Version() {
		t.Fatal("same version twice")
	}
	old, err := c.GetSecret(ctx, "rot", v1.ID.Version(), nil)
	if err != nil || *old.Value != "one" {
		t.Fatalf("old version: %v %v", old.Value, err)
	}
	latest, _ := c.GetSecret(ctx, "rot", "", nil)
	if *latest.Value != "two" {
		t.Fatalf("latest %q", *latest.Value)
	}
	n := 0
	pager := c.NewListSecretPropertiesVersionsPager("rot", nil)
	for pager.More() {
		pg, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Value)
	}
	if n != 2 {
		t.Fatalf("versions = %d", n)
	}
}

func TestSecret_GetMissingIsSecretNotFound404(t *testing.T) {
	c := secretsClient(t, newVault())
	_, err := c.GetSecret(context.Background(), "nope", "", nil)
	respErr(t, err, http.StatusNotFound, "SecretNotFound")
}

func TestSecret_UpdatePropertiesChangesContentTypeAndTagsNotValue(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	set, _ := c.SetSecret(ctx, "props", azsecrets.SetSecretParameters{Value: ptr("v")}, nil)
	up, err := c.UpdateSecretProperties(ctx, "props", set.ID.Version(), azsecrets.UpdateSecretPropertiesParameters{
		ContentType: ptr("text/plain"), Tags: map[string]*string{"env": ptr("test")},
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if up.ContentType == nil || *up.ContentType != "text/plain" || *up.Tags["env"] != "test" {
		t.Fatalf("update result %+v", up)
	}
	got, _ := c.GetSecret(ctx, "props", "", nil)
	if *got.Value != "v" || *got.ContentType != "text/plain" {
		t.Fatalf("got %+v", got)
	}
}

func TestSecret_DisabledSecretCannotBeRead403(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	set, _ := c.SetSecret(ctx, "dis", azsecrets.SetSecretParameters{Value: ptr("v"),
		SecretAttributes: &azsecrets.SecretAttributes{Enabled: ptr(false)}}, nil)
	_ = set
	_, err := c.GetSecret(ctx, "dis", "", nil)
	respErrInner(t, err, http.StatusForbidden, "Forbidden", "SecretDisabled")
}

func TestSecret_NotBeforeAndExpiryRoundTrip(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	nbf := time.Date(2030, 1, 2, 3, 4, 5, 0, time.UTC)
	exp := time.Date(2031, 1, 2, 3, 4, 5, 0, time.UTC)
	_, err := c.SetSecret(ctx, "dates", azsecrets.SetSecretParameters{Value: ptr("v"),
		SecretAttributes: &azsecrets.SecretAttributes{NotBefore: &nbf, Expires: &exp}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	got, _ := c.GetSecret(ctx, "dates", "", nil)
	if !got.Attributes.NotBefore.Equal(nbf) || !got.Attributes.Expires.Equal(exp) {
		t.Fatalf("dates %v %v", got.Attributes.NotBefore, got.Attributes.Expires)
	}
}

func TestSecret_DeleteThenRecoverKeepsValue(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	c.SetSecret(ctx, "del", azsecrets.SetSecretParameters{Value: ptr("keep")}, nil)
	d, err := c.DeleteSecret(ctx, "del", nil)
	if err != nil {
		t.Fatal(err)
	}
	if d.RecoveryID == nil || d.DeletedDate == nil || d.ScheduledPurgeDate == nil {
		t.Fatalf("delete result lacks recovery info: %+v", d)
	}
	_, err = c.GetSecret(ctx, "del", "", nil)
	respErr(t, err, http.StatusNotFound, "SecretNotFound")
	if _, err := c.GetDeletedSecret(ctx, "del", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := c.RecoverDeletedSecret(ctx, "del", nil); err != nil {
		t.Fatal(err)
	}
	got, err := c.GetSecret(ctx, "del", "", nil)
	if err != nil || *got.Value != "keep" {
		t.Fatalf("after recover: %v %v", got.Value, err)
	}
}

func TestSecret_NameOfADeletedSecretCannotBeReusedUntilPurged(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	c.SetSecret(ctx, "reuse", azsecrets.SetSecretParameters{Value: ptr("a")}, nil)
	c.DeleteSecret(ctx, "reuse", nil)
	_, err := c.SetSecret(ctx, "reuse", azsecrets.SetSecretParameters{Value: ptr("b")}, nil)
	respErrInner(t, err, http.StatusConflict, "Conflict", "ObjectIsDeletedButRecoverable")
	if _, err := c.PurgeDeletedSecret(ctx, "reuse", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := c.SetSecret(ctx, "reuse", azsecrets.SetSecretParameters{Value: ptr("b")}, nil); err != nil {
		t.Fatalf("after purge: %v", err)
	}
}

func TestSecret_ListPagesThroughMoreThanOnePage(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	const total = 30
	for i := 0; i < total; i++ {
		if _, err := c.SetSecret(ctx, fmt.Sprintf("s%03d", i), azsecrets.SetSecretParameters{Value: ptr("v")}, nil); err != nil {
			t.Fatal(err)
		}
	}
	pager := c.NewListSecretPropertiesPager(nil)
	pages, n := 0, 0
	var names []string
	for pager.More() {
		pg, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		for _, s := range pg.Value {
			names = append(names, s.ID.Name())
		}
		n += len(pg.Value)
	}
	if n != total || pages < 2 {
		t.Fatalf("n=%d pages=%d", n, pages)
	}
	for i, nm := range names {
		if nm != fmt.Sprintf("s%03d", i) {
			t.Fatalf("order: %v", names)
		}
	}
}

func TestSecret_BackupThenRestoreIntoAnotherVault(t *testing.T) {
	ctx := context.Background()
	a := secretsClient(t, newVault())
	b := secretsClient(t, newVault())
	a.SetSecret(ctx, "bk", azsecrets.SetSecretParameters{Value: ptr("payload")}, nil)
	bk, err := a.BackupSecret(ctx, "bk", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(bk.Value) == 0 {
		t.Fatal("empty backup")
	}
	if _, err := b.RestoreSecret(ctx, azsecrets.RestoreSecretParameters{SecretBackup: bk.Value}, nil); err != nil {
		t.Fatal(err)
	}
	got, err := b.GetSecret(ctx, "bk", "", nil)
	if err != nil || *got.Value != "payload" {
		t.Fatalf("restored: %v %v", got.Value, err)
	}
}

func TestSecret_InvalidNameIsBadParameter400(t *testing.T) {
	c := secretsClient(t, newVault())
	_, err := c.SetSecret(context.Background(), "bad_name", azsecrets.SetSecretParameters{Value: ptr("v")}, nil)
	respErr(t, err, http.StatusBadRequest, "BadParameter")
}

func TestSecret_LargeValueRoundTripsAndUnicode(t *testing.T) {
	ctx := context.Background()
	c := secretsClient(t, newVault())
	val := "héllo ✓ 日本語 " + string(make([]rune, 0))
	for i := 0; i < 2000; i++ {
		val += "x"
	}
	c.SetSecret(ctx, "big", azsecrets.SetSecretParameters{Value: &val}, nil)
	got, err := c.GetSecret(ctx, "big", "", nil)
	if err != nil || *got.Value != val {
		t.Fatalf("round trip differs: %v", err)
	}
}

var _ = emu.E
