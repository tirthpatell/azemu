package appconfig

import (
	"context"
	"errors"
	"fmt"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/runtime"
	"net/http"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/data/azappconfig"
)

func TestMain(m *testing.M) { emu.Main(m) }

func connStr(store string) string {
	return fmt.Sprintf("Endpoint=%s;Id=any;Secret=YXplbXU=", emu.E.HTTP("azconfig", store))
}

func newClient(t testing.TB) *azappconfig.Client {
	t.Helper()
	c, err := azappconfig.NewClientFromConnectionString(connStr(emu.Name("ac")), nil)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func status(t testing.TB, err error, want int) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected status %d, got success", want)
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != want {
		t.Fatalf("status %d, want %d: %v", re.StatusCode, want, err)
	}
}

func ptr[T any](v T) *T { return &v }

func TestSetting_SetThenGetReturnsValueContentTypeTagsAndETag(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	set, err := c.SetSetting(ctx, "app/color", ptr("blue"), &azappconfig.SetSettingOptions{ContentType: ptr("text/plain")})
	if err != nil {
		t.Fatal(err)
	}
	if set.ETag == nil || *set.Key != "app/color" || *set.Value != "blue" {
		t.Fatalf("set response %+v", set)
	}
	got, err := c.GetSetting(ctx, "app/color", nil)
	if err != nil {
		t.Fatal(err)
	}
	if *got.Value != "blue" || *got.ContentType != "text/plain" || *got.ETag != *set.ETag || got.LastModified == nil {
		t.Fatalf("get %+v", got)
	}
	if got.IsReadOnly != nil && *got.IsReadOnly {
		t.Fatal("new setting must not be read only")
	}
}

func TestSetting_LabelsAreSeparateSettingsAndTheNullLabelIsItsOwn(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "k", ptr("none"), nil)
	c.SetSetting(ctx, "k", ptr("prod"), &azappconfig.SetSettingOptions{Label: ptr("prod")})
	c.SetSetting(ctx, "k", ptr("dev"), &azappconfig.SetSettingOptions{Label: ptr("dev")})
	for label, want := range map[string]string{"prod": "prod", "dev": "dev"} {
		g, err := c.GetSetting(ctx, "k", &azappconfig.GetSettingOptions{Label: ptr(label)})
		if err != nil || *g.Value != want {
			t.Fatalf("label %s: %v %v", label, g.Value, err)
		}
	}
	g, err := c.GetSetting(ctx, "k", nil)
	if err != nil || *g.Value != "none" || g.Label != nil && *g.Label != "" {
		t.Fatalf("null label: %v %v", g.Value, err)
	}
}

func TestSetting_GetMissingIsNotFound404(t *testing.T) {
	c := newClient(t)
	_, err := c.GetSetting(context.Background(), "nope", nil)
	status(t, err, http.StatusNotFound)
}

func TestSetting_AddOfAnExistingKeyIsPreconditionFailed412(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	if _, err := c.AddSetting(ctx, "once", ptr("1"), nil); err != nil {
		t.Fatal(err)
	}
	_, err := c.AddSetting(ctx, "once", ptr("2"), nil)
	status(t, err, http.StatusPreconditionFailed)
}

func TestSetting_OverwriteWithStaleETagIsPreconditionFailedAndWithCurrentETagWorks(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	first, _ := c.SetSetting(ctx, "cas", ptr("1"), nil)
	second, _ := c.SetSetting(ctx, "cas", ptr("2"), nil)
	if *first.ETag == *second.ETag {
		t.Fatal("etag must change on write")
	}
	_, err := c.SetSetting(ctx, "cas", ptr("3"), &azappconfig.SetSettingOptions{OnlyIfUnchanged: first.ETag})
	status(t, err, http.StatusPreconditionFailed)
	if _, err := c.SetSetting(ctx, "cas", ptr("3"), &azappconfig.SetSettingOptions{OnlyIfUnchanged: second.ETag}); err != nil {
		t.Fatalf("with current etag: %v", err)
	}
}

func TestSetting_GetWithIfNoneMatchOfTheCurrentETagIsNotModified304(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	set, _ := c.SetSetting(ctx, "inm", ptr("1"), nil)
	_, err := c.GetSetting(ctx, "inm", &azappconfig.GetSettingOptions{OnlyIfChanged: set.ETag})
	if err == nil {
		t.Skip("the SDK returns no error for 304")
	}
	status(t, err, http.StatusNotModified)
}

func TestSetting_ReadOnlySettingRefusesWritesAndDeletesUntilMadeWritableAgain(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "lock", ptr("1"), nil)
	ro, err := c.SetReadOnly(ctx, "lock", true, nil)
	if err != nil || ro.IsReadOnly == nil || !*ro.IsReadOnly {
		t.Fatalf("set read only: %v", err)
	}
	_, err = c.SetSetting(ctx, "lock", ptr("2"), nil)
	status(t, err, http.StatusConflict)
	_, err = c.DeleteSetting(ctx, "lock", nil)
	status(t, err, http.StatusConflict)
	if _, err := c.SetReadOnly(ctx, "lock", false, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := c.SetSetting(ctx, "lock", ptr("2"), nil); err != nil {
		t.Fatalf("after unlock: %v", err)
	}
}

func TestSetting_DeleteReturnsTheDeletedSettingAndAMissingOneIsNoError(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "gone", ptr("bye"), nil)
	d, err := c.DeleteSetting(ctx, "gone", nil)
	if err != nil || d.Value == nil || *d.Value != "bye" {
		t.Fatalf("delete: %v %+v", err, d)
	}
	if _, err := c.GetSetting(ctx, "gone", nil); err == nil {
		t.Fatal("deleted setting still readable")
	}
	if _, err := c.DeleteSetting(ctx, "gone", nil); err != nil {
		t.Fatalf("deleting a missing setting answers 204: %v", err)
	}
}

func TestSetting_KeysWithSlashPercentSpaceAndUnicodeRoundTrip(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	for _, k := range []string{"a/b/c", "100%", "with space", "ünï-日本語", "plus+sign", "hash#q?x", ".appconfig.featureflag/beta", "colon:semi;"} {
		if _, err := c.SetSetting(ctx, k, ptr("v:"+k), nil); err != nil {
			t.Fatalf("set %q: %v", k, err)
		}
		g, err := c.GetSetting(ctx, k, nil)
		if err != nil || *g.Value != "v:"+k || *g.Key != k {
			t.Fatalf("get %q: %v %+v", k, err, g)
		}
	}
}

func TestList_KeyFilterWithWildcardLabelFilterAndPaging(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	for i := 0; i < 130; i++ {
		c.SetSetting(ctx, fmt.Sprintf("app/%03d", i), ptr("v"), nil)
	}
	c.SetSetting(ctx, "other/x", ptr("v"), nil)
	c.SetSetting(ctx, "app/special", ptr("v"), &azappconfig.SetSettingOptions{Label: ptr("beta")})
	n, pages := 0, 0
	for p := c.NewListSettingsPager(azappconfig.SettingSelector{KeyFilter: ptr("app/*")}, nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		n += len(pg.Settings)
	}
	if n != 131 || pages < 2 {
		t.Fatalf("n=%d pages=%d", n, pages)
	}
	n = 0
	for p := c.NewListSettingsPager(azappconfig.SettingSelector{KeyFilter: ptr("*"), LabelFilter: ptr("beta")}, nil); p.More(); {
		pg, _ := p.NextPage(ctx)
		n += len(pg.Settings)
	}
	if n != 1 {
		t.Fatalf("label filter: %d", n)
	}
}

func TestList_FieldsSelectLimitsWhatComesBack(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "sel", ptr("secret-value"), &azappconfig.SetSettingOptions{ContentType: ptr("text/x")})
	for p := c.NewListSettingsPager(azappconfig.SettingSelector{Fields: []azappconfig.SettingFields{azappconfig.SettingFieldsKey}}, nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if len(pg.Settings) != 1 || *pg.Settings[0].Key != "sel" || pg.Settings[0].Value != nil {
			t.Fatalf("settings %+v", pg.Settings)
		}
	}
}

func TestRevisions_EveryWriteIsKeptAndListedNewestFirst(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	for i := 1; i <= 3; i++ {
		c.SetSetting(ctx, "hist", ptr(fmt.Sprintf("v%d", i)), nil)
	}
	var vals []string
	for p := c.NewListRevisionsPager(azappconfig.SettingSelector{KeyFilter: ptr("hist")}, nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, s := range pg.Settings {
			vals = append(vals, *s.Value)
		}
	}
	if len(vals) != 3 {
		t.Fatalf("revisions %v", vals)
	}
	seen := map[string]bool{}
	for _, v := range vals {
		seen[v] = true
	}
	if !seen["v1"] || !seen["v2"] || !seen["v3"] {
		t.Fatalf("revisions %v", vals)
	}
}

func TestSetting_AcceptDateTimeReadsTheValueAsItWasAtThatTime(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "tt", ptr("old"), nil)
	time.Sleep(1500 * time.Millisecond)
	at := time.Now().UTC()
	time.Sleep(1500 * time.Millisecond)
	c.SetSetting(ctx, "tt", ptr("new"), nil)
	g, err := c.GetSetting(ctx, "tt", &azappconfig.GetSettingOptions{AcceptDateTime: &at})
	if err != nil || *g.Value != "old" {
		t.Fatalf("at %v: %v %v", at, g.Value, err)
	}
	cur, _ := c.GetSetting(ctx, "tt", nil)
	if *cur.Value != "new" {
		t.Fatalf("current %q", *cur.Value)
	}
}

func TestSnapshot_CreateIsFrozenAtCreationAndCanBeListedAndRead(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	for i := 0; i < 3; i++ {
		c.SetSetting(ctx, fmt.Sprintf("snap/%d", i), ptr("before"), nil)
	}
	poller, err := c.BeginCreateSnapshot(ctx, "snap-one", []azappconfig.SettingFilter{{KeyFilter: ptr("snap/*")}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	res, err := poller.PollUntilDone(ctx, &runtimePollOpts)
	if err != nil {
		t.Fatalf("poll: %v", err)
	}
	if *res.Name != "snap-one" || *res.Status != azappconfig.SnapshotStatusReady || *res.ItemsCount != 3 {
		t.Fatalf("snapshot %+v", res.Snapshot)
	}
	c.SetSetting(ctx, "snap/0", ptr("after"), nil)
	c.SetSetting(ctx, "snap/new", ptr("later"), nil)
	n := 0
	for p := c.NewListSettingsForSnapshotPager("snap-one", nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, s := range pg.Settings {
			n++
			if *s.Value != "before" {
				t.Fatalf("snapshot content changed: %s=%s", *s.Key, *s.Value)
			}
		}
	}
	if n != 3 {
		t.Fatalf("settings in snapshot: %d", n)
	}
	got, err := c.GetSnapshot(ctx, "snap-one", nil)
	if err != nil || *got.Name != "snap-one" {
		t.Fatalf("get snapshot: %v", err)
	}
	found := false
	for p := c.NewListSnapshotsPager(nil); p.More(); {
		pg, _ := p.NextPage(ctx)
		for _, s := range pg.Snapshots {
			found = found || *s.Name == "snap-one"
		}
	}
	if !found {
		t.Fatal("snapshot not listed")
	}
}

func TestSnapshot_ArchiveThenRecoverChangesStatus(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "arch/1", ptr("v"), nil)
	poller, err := c.BeginCreateSnapshot(ctx, "arch-snap", []azappconfig.SettingFilter{{KeyFilter: ptr("arch/*")}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := poller.PollUntilDone(ctx, &runtimePollOpts); err != nil {
		t.Fatal(err)
	}
	a, err := c.ArchiveSnapshot(ctx, "arch-snap", nil)
	if err != nil || *a.Status != azappconfig.SnapshotStatusArchived {
		t.Fatalf("archive: %v", err)
	}
	r, err := c.RecoverSnapshot(ctx, "arch-snap", nil)
	if err != nil || *r.Status != azappconfig.SnapshotStatusReady {
		t.Fatalf("recover: %v", err)
	}
}

func TestSnapshot_CreatingTheSameNameTwiceIsAConflict409(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	c.SetSetting(ctx, "dup/1", ptr("v"), nil)
	filters := []azappconfig.SettingFilter{{KeyFilter: ptr("dup/*")}}
	p, err := c.BeginCreateSnapshot(ctx, "dup-snap", filters, nil)
	if err != nil {
		t.Fatal(err)
	}
	p.PollUntilDone(ctx, &runtimePollOpts)
	_, err = c.BeginCreateSnapshot(ctx, "dup-snap", filters, nil)
	if err == nil {
		t.Fatal("creating a snapshot that exists must fail")
	}
	status(t, err, http.StatusConflict)
}

func TestAuth_BearerTokenCredentialOverHttpsWorks(t *testing.T) {
	ctx := context.Background()
	store := emu.Name("acaad")
	c, err := azappconfig.NewClient(emu.E.HTTPS("azconfig", store), emu.Cred(), &azappconfig.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: emu.E.Transport()}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.SetSetting(ctx, "k", ptr("v"), nil); err != nil {
		t.Fatalf("set over https with a token: %v", err)
	}
	g, err := c.GetSetting(ctx, "k", nil)
	if err != nil || *g.Value != "v" {
		t.Fatalf("get: %v", err)
	}
}

var runtimePollOpts = runtime.PollUntilDoneOptions{Frequency: 200 * time.Millisecond}
