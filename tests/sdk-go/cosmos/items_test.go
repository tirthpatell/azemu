package cosmos

import (
	"context"
	"fmt"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
)

func TestItem_CreateThenReadReturnsTheDocumentWithSystemProperties(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("tenant-1")
	create, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": "a1", "pk": "tenant-1", "name": "Ada", "n": 42, "nested": map[string]any{"x": []int{1, 2, 3}}}), &azcosmos.ItemOptions{EnableContentResponseOnWrite: true})
	if err != nil {
		t.Fatal(err)
	}
	if create.RawResponse.StatusCode != http.StatusCreated || create.ETag == "" {
		t.Fatalf("create: status %d etag %q", create.RawResponse.StatusCode, create.ETag)
	}
	got, err := ct.ReadItem(ctx, pk, "a1", nil)
	if err != nil {
		t.Fatal(err)
	}
	m := parse(t, got.Value)
	if m["name"] != "Ada" || m["n"] != float64(42) || m["_rid"] == nil || m["_etag"] == nil || m["_ts"] == nil || m["_self"] == nil {
		t.Fatalf("document %v", m)
	}
	if got.ETag != create.ETag {
		t.Fatalf("etag %q vs %q", got.ETag, create.ETag)
	}
	if got.RequestCharge <= 0 {
		t.Fatal("request charge missing")
	}
}

func TestItem_CreateWithoutContentResponseOnWriteReturnsNoBody(t *testing.T) {
	ct := newContainer(t, newClient(t))
	r, err := ct.CreateItem(context.Background(), azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": "x", "pk": "p"}), nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(r.Value) != 0 {
		t.Fatalf("expected no body, got %s", r.Value)
	}
}

func TestItem_CreateDuplicateIdInTheSamePartitionIsConflict409ButOtherPartitionWorks(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p1"), doc(map[string]any{"id": "same", "pk": "p1"}), nil)
	_, err := ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p1"), doc(map[string]any{"id": "same", "pk": "p1"}), nil)
	status(t, err, http.StatusConflict)
	if _, err := ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p2"), doc(map[string]any{"id": "same", "pk": "p2"}), nil); err != nil {
		t.Fatalf("same id in another partition: %v", err)
	}
}

func TestItem_ReadMissingIsNotFound404(t *testing.T) {
	ct := newContainer(t, newClient(t))
	_, err := ct.ReadItem(context.Background(), azcosmos.NewPartitionKeyString("p"), "ghost", nil)
	status(t, err, http.StatusNotFound)
}

func TestItem_WrongPartitionKeyValueFindsNothing(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("right"), doc(map[string]any{"id": "i", "pk": "right"}), nil)
	_, err := ct.ReadItem(ctx, azcosmos.NewPartitionKeyString("wrong"), "i", nil)
	status(t, err, http.StatusNotFound)
}

func TestItem_PartitionKeyHeaderThatDisagreesWithTheBodyIsBadRequest400(t *testing.T) {
	ct := newContainer(t, newClient(t))
	_, err := ct.CreateItem(context.Background(), azcosmos.NewPartitionKeyString("header"), doc(map[string]any{"id": "i", "pk": "body"}), nil)
	status(t, err, http.StatusBadRequest)
}

func TestItem_ReplaceWithIfMatchStaleIsPreconditionFailed412AndCurrentWorks(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	first, _ := ct.CreateItem(ctx, pk, doc(map[string]any{"id": "i", "pk": "p", "v": 1}), nil)
	second, err := ct.ReplaceItem(ctx, pk, "i", doc(map[string]any{"id": "i", "pk": "p", "v": 2}), nil)
	if err != nil {
		t.Fatal(err)
	}
	if first.ETag == second.ETag {
		t.Fatal("etag must change")
	}
	_, err = ct.ReplaceItem(ctx, pk, "i", doc(map[string]any{"id": "i", "pk": "p", "v": 3}), &azcosmos.ItemOptions{IfMatchEtag: &first.ETag})
	status(t, err, http.StatusPreconditionFailed)
	if _, err := ct.ReplaceItem(ctx, pk, "i", doc(map[string]any{"id": "i", "pk": "p", "v": 3}), &azcosmos.ItemOptions{IfMatchEtag: &second.ETag}); err != nil {
		t.Fatalf("with current etag: %v", err)
	}
}

func TestItem_DeleteWithStaleETagIs412AndWithoutETagRemovesIt(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	first, _ := ct.CreateItem(ctx, pk, doc(map[string]any{"id": "i", "pk": "p"}), nil)
	ct.ReplaceItem(ctx, pk, "i", doc(map[string]any{"id": "i", "pk": "p", "v": 2}), nil)
	_, err := ct.DeleteItem(ctx, pk, "i", &azcosmos.ItemOptions{IfMatchEtag: &first.ETag})
	status(t, err, http.StatusPreconditionFailed)
	r, err := ct.DeleteItem(ctx, pk, "i", nil)
	if err != nil || r.RawResponse.StatusCode != http.StatusNoContent {
		t.Fatalf("delete: %v", err)
	}
	_, err = ct.ReadItem(ctx, pk, "i", nil)
	status(t, err, http.StatusNotFound)
	_, err = ct.DeleteItem(ctx, pk, "i", nil)
	status(t, err, http.StatusNotFound)
}

func TestItem_UpsertCreatesWith201ThenReplacesWith200(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	a, err := ct.UpsertItem(ctx, pk, doc(map[string]any{"id": "u", "pk": "p", "v": 1}), nil)
	if err != nil || a.RawResponse.StatusCode != http.StatusCreated {
		t.Fatalf("first upsert: %v", err)
	}
	b, err := ct.UpsertItem(ctx, pk, doc(map[string]any{"id": "u", "pk": "p", "v": 2}), nil)
	if err != nil || b.RawResponse.StatusCode != http.StatusOK {
		t.Fatalf("second upsert: %v", err)
	}
	got, _ := ct.ReadItem(ctx, pk, "u", nil)
	if parse(t, got.Value)["v"] != float64(2) {
		t.Fatalf("%s", got.Value)
	}
}

func TestItem_IdsWithSpacesPercentPlusUnicodeAndAtSignRoundTrip(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	for _, id := range []string{"with space", "100%", "plus+sign", "ünï-日本語", "at@sign", "amp&eq=semi;", "colon:comma,", "dots...", "emoji-😀"} {
		if _, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": id, "pk": "p"}), nil); err != nil {
			t.Fatalf("create %q: %v", id, err)
		}
		r, err := ct.ReadItem(ctx, pk, id, nil)
		if err != nil {
			t.Fatalf("read %q: %v", id, err)
		}
		if parse(t, r.Value)["id"] != id {
			t.Fatalf("id %q came back as %s", id, r.Value)
		}
		if _, err := ct.DeleteItem(ctx, pk, id, nil); err != nil {
			t.Fatalf("delete %q: %v", id, err)
		}
	}
}

// Azure's service refuses only '/' and backslash in an item id; '?' and '#' are accepted (item_ids_test.go).
func TestItem_IdWithForbiddenCharacterIsBadRequest400(t *testing.T) {
	ct := newContainer(t, newClient(t))
	for _, id := range []string{"a/b", `a\b`} {
		_, err := ct.CreateItem(context.Background(), azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": id, "pk": "p"}), nil)
		status(t, err, http.StatusBadRequest)
	}
}

func TestItem_PartitionKeyTypesNumberBoolStringAndNullAreDistinctPartitions(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	cases := []struct {
		key  azcosmos.PartitionKey
		body any
	}{
		{azcosmos.NewPartitionKeyString("1"), "1"},
		{azcosmos.NewPartitionKeyNumber(1), float64(1)},
		{azcosmos.NewPartitionKeyBool(true), true},
		{azcosmos.NewPartitionKey().AppendNull(), nil},
	}
	for i, c := range cases {
		if _, err := ct.CreateItem(ctx, c.key, doc(map[string]any{"id": "same", "pk": c.body, "which": i}), nil); err != nil {
			t.Fatalf("create with key #%d: %v", i, err)
		}
	}
	for i, c := range cases {
		r, err := ct.ReadItem(ctx, c.key, "same", nil)
		if err != nil {
			t.Fatalf("read #%d: %v", i, err)
		}
		if parse(t, r.Value)["which"] != float64(i) {
			t.Fatalf("partition #%d returned %s", i, r.Value)
		}
	}
}

func TestItem_HierarchicalPartitionKeyNeedsAllLevelsAndSeparatesPartitions(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	ct := newContainerWith(t, c, azcosmos.ContainerProperties{PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{
		Paths: []string{"/tenant", "/user"}, Kind: azcosmos.PartitionKeyKindMultiHash, Version: 2}}, nil)
	pk := func(a, b string) azcosmos.PartitionKey { return azcosmos.NewPartitionKeyString(a).AppendString(b) }
	for _, k := range [][2]string{{"t1", "u1"}, {"t1", "u2"}, {"t2", "u1"}} {
		if _, err := ct.CreateItem(ctx, pk(k[0], k[1]), doc(map[string]any{"id": "doc", "tenant": k[0], "user": k[1]}), nil); err != nil {
			t.Fatalf("create %v: %v", k, err)
		}
	}
	r, err := ct.ReadItem(ctx, pk("t1", "u2"), "doc", nil)
	if err != nil || parse(t, r.Value)["user"] != "u2" {
		t.Fatalf("read: %v", err)
	}
	// query one logical partition with the full key, then by the first level only (a prefix)
	got, _ := queryAll(t, ct, "SELECT * FROM c", pk("t1", "u1"), nil)
	if len(got) != 1 {
		t.Fatalf("full key query: %d", len(got))
	}
	got, _ = queryAll(t, ct, "SELECT * FROM c WHERE c.tenant = 't1'", azcosmos.NewPartitionKey(), nil)
	if len(got) != 2 {
		t.Fatalf("query on the first level: %d", len(got))
	}
	// a key with too few levels is refused
	_, err = ct.ReadItem(ctx, azcosmos.NewPartitionKeyString("t1"), "doc", nil)
	if err == nil {
		t.Fatal("reading with a partial hierarchical key must fail")
	}
}

func TestItem_UniqueKeyPolicyRefusesASecondItemWithTheSameValueButAllowsAnotherPartition(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	ct := newContainerWith(t, c, azcosmos.ContainerProperties{
		PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}},
		UniqueKeyPolicy:        &azcosmos.UniqueKeyPolicy{UniqueKeys: []azcosmos.UniqueKey{{Paths: []string{"/email"}}}},
	}, nil)
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "1", "pk": "p", "email": "a@x"}), nil)
	_, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": "2", "pk": "p", "email": "a@x"}), nil)
	status(t, err, http.StatusConflict)
	if _, err := ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("q"), doc(map[string]any{"id": "3", "pk": "q", "email": "a@x"}), nil); err != nil {
		t.Fatalf("other partition: %v", err)
	}
}

func TestItem_TimeToLiveOfOneSecondRemovesTheItem(t *testing.T) {
	ctx := context.Background()
	c := newClient(t)
	ct := newContainerWith(t, c, azcosmos.ContainerProperties{
		PartitionKeyDefinition: azcosmos.PartitionKeyDefinition{Paths: []string{"/pk"}}, DefaultTimeToLive: ptr(int32(-1))}, nil)
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "short", "pk": "p", "ttl": 1}), nil)
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "long", "pk": "p"}), nil)
	time.Sleep(2500 * time.Millisecond)
	_, err := ct.ReadItem(ctx, pk, "short", nil)
	status(t, err, http.StatusNotFound)
	if _, err := ct.ReadItem(ctx, pk, "long", nil); err != nil {
		t.Fatalf("item without ttl: %v", err)
	}
}

func TestItem_BodyOverTwoMegabytesIsRequestEntityTooLarge413(t *testing.T) {
	ct := newContainer(t, newClient(t))
	_, err := ct.CreateItem(context.Background(), azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": "big", "pk": "p", "blob": strings.Repeat("x", 2*1024*1024+10)}), nil)
	status(t, err, http.StatusRequestEntityTooLarge)
}

func TestItem_LargeNumbersAndUnicodeStringsRoundTrip(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	body := `{"id":"n","pk":"p","big":9007199254740991,"neg":-0.5,"exp":1.5e300,"s":"ünï 日本語 😀 \" \\ \n","nul":null,"arr":[1,"a",null,{"k":true}]}`
	if _, err := ct.CreateItem(ctx, pk, []byte(body), nil); err != nil {
		t.Fatal(err)
	}
	r, _ := ct.ReadItem(ctx, pk, "n", nil)
	m := parse(t, r.Value)
	if m["big"] != float64(9007199254740991) || m["neg"] != -0.5 || m["exp"] != 1.5e300 || m["s"] != "ünï 日本語 😀 \" \\ \n" || m["nul"] != nil {
		t.Fatalf("round trip: %v", m)
	}
}

func TestItem_ReadManyReturnsEveryRequestedItemAcrossPartitions(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	var ids []azcosmos.ItemIdentity
	for i := 0; i < 6; i++ {
		p := fmt.Sprintf("p%d", i%3)
		id := fmt.Sprintf("m%d", i)
		ct.CreateItem(ctx, azcosmos.NewPartitionKeyString(p), doc(map[string]any{"id": id, "pk": p}), nil)
		ids = append(ids, azcosmos.ItemIdentity{ID: id, PartitionKey: azcosmos.NewPartitionKeyString(p)})
	}
	ids = append(ids, azcosmos.ItemIdentity{ID: "missing", PartitionKey: azcosmos.NewPartitionKeyString("p0")})
	r, err := ct.ReadManyItems(ctx, ids, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(r.Items) != 6 {
		t.Fatalf("read many returned %d items, want 6 (the missing one is skipped)", len(r.Items))
	}
}

func TestAuth_EntraIdTokenCredentialWorksLikeAKey(t *testing.T) {
	ctx := context.Background()
	c, err := azcosmos.NewClient(endpoint(), emu.Cred(), &azcosmos.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: emu.E.Transport()}})
	if err != nil {
		t.Fatal(err)
	}
	db := newDB(t, c)
	if _, err := db.Read(ctx, nil); err != nil {
		t.Fatalf("read with a bearer token: %v", err)
	}
}

func TestAuth_ConnectionStringClient(t *testing.T) {
	ctx := context.Background()
	c, err := azcosmos.NewClientFromConnectionString(fmt.Sprintf("AccountEndpoint=%s;AccountKey=%s;", endpoint(), cosmosKey),
		&azcosmos.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: emu.E.Transport()}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.CreateDatabase(ctx, azcosmos.DatabaseProperties{ID: emu.Name("cs")}, nil); err != nil {
		t.Fatal(err)
	}
}
