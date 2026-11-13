package tables

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"sync"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/data/aztables"
)

func TestMain(m *testing.M) { emu.Main(m) }

func cred(t testing.TB) *aztables.SharedKeyCredential {
	t.Helper()
	c, err := aztables.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func svc(t testing.TB) *aztables.ServiceClient {
	t.Helper()
	s, err := aztables.NewServiceClientWithSharedKey(emu.E.HTTP("table", emu.DevAccount)+"/", cred(t), &aztables.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: &http.Client{Transport: emu.Dump(emu.E.NewTransport())}}})
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func newTable(t testing.TB, prefix string) *aztables.Client {
	t.Helper()
	name := emu.Name(prefix)
	c := svc(t).NewClient(name)
	names.Store(c, name)
	if _, err := c.CreateTable(context.Background(), nil); err != nil {
		t.Fatalf("create table: %v", err)
	}
	return c
}

func wantCode(t testing.TB, err error, status int, code aztables.TableErrorCode) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected %d %s, got success", status, code)
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != status || re.ErrorCode != string(code) {
		t.Fatalf("got %d %q, want %d %q", re.StatusCode, re.ErrorCode, status, code)
	}
}

func entity(pk, rk string, props map[string]any) []byte {
	b, err := json.Marshal(aztables.EDMEntity{Entity: aztables.Entity{PartitionKey: pk, RowKey: rk}, Properties: props})
	if err != nil {
		panic(err)
	}
	return b
}

func get(t testing.TB, c *aztables.Client, pk, rk string) aztables.EDMEntity {
	t.Helper()
	r, err := c.GetEntity(context.Background(), pk, rk, nil)
	if err != nil {
		t.Fatal(err)
	}
	var e aztables.EDMEntity
	if err := json.Unmarshal(r.Value, &e); err != nil {
		t.Fatal(err)
	}
	return e
}

func queryAll(t testing.TB, c *aztables.Client, o *aztables.ListEntitiesOptions) ([]aztables.EDMEntity, int) {
	t.Helper()
	var out []aztables.EDMEntity
	pages := 0
	for p := c.NewListEntitiesPager(o); p.More(); {
		pg, err := p.NextPage(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		pages++
		for _, raw := range pg.Entities {
			var e aztables.EDMEntity
			if err := json.Unmarshal(raw, &e); err != nil {
				t.Fatal(err)
			}
			out = append(out, e)
		}
	}
	return out, pages
}

func ptr[T any](v T) *T { return &v }

func TestTable_CreateTwiceIsTableAlreadyExists409(t *testing.T) {
	c := newTable(t, "dup")
	_, err := c.CreateTable(context.Background(), nil)
	wantCode(t, err, http.StatusConflict, aztables.TableAlreadyExists)
}

func TestTable_DeleteThenAddEntityIsTableNotFound404(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "del")
	if _, err := c.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := c.AddEntity(ctx, entity("p", "r", nil), nil)
	wantCode(t, err, http.StatusNotFound, aztables.TableNotFound)
}

func TestTable_ListTablesWithPrefixFilterAndPaging(t *testing.T) {
	ctx := context.Background()
	s := svc(t)
	pre := emu.Name("lt")
	for i := 0; i < 5; i++ {
		s.CreateTable(ctx, fmt.Sprintf("%s%d", pre, i), nil)
	}
	filter := fmt.Sprintf("TableName ge '%s' and TableName lt '%sz'", pre, pre)
	n, pages := 0, 0
	for p := s.NewListTablesPager(&aztables.ListTablesOptions{Filter: &filter, Top: ptr(int32(2))}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		n += len(pg.Tables)
	}
	if n != 5 || pages != 3 {
		t.Fatalf("n=%d pages=%d", n, pages)
	}
}

func TestEntity_EveryEdmTypeRoundTripsWithItsType(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "edm")
	when := time.Date(2024, 2, 29, 13, 14, 15, 123456700, time.UTC)
	guid := "0d391d16-97f1-4b9a-be68-4cc871f90001"
	props := map[string]any{
		"Str":   "héllo ✓",
		"Int32": int32(-42),
		"Int64": aztables.EDMInt64(9007199254740993), // above 2^53: loses precision as a JSON number
		"Float": 3.5,
		"Bool":  true,
		"Bin":   aztables.EDMBinary([]byte{0, 1, 2, 254, 255}),
		"Guid":  aztables.EDMGUID(guid),
		"When":  aztables.EDMDateTime(when),
	}
	if _, err := c.AddEntity(ctx, entity("p", "r", props), nil); err != nil {
		t.Fatal(err)
	}
	e := get(t, c, "p", "r")
	if e.Properties["Str"] != "héllo ✓" || e.Properties["Int32"] != int32(-42) || e.Properties["Float"] != 3.5 || e.Properties["Bool"] != true {
		t.Fatalf("simple types: %#v", e.Properties)
	}
	if e.Properties["Int64"] != aztables.EDMInt64(9007199254740993) {
		t.Fatalf("int64: %#v", e.Properties["Int64"])
	}
	if string(e.Properties["Bin"].(aztables.EDMBinary)) != string([]byte{0, 1, 2, 254, 255}) {
		t.Fatalf("binary: %#v", e.Properties["Bin"])
	}
	if e.Properties["Guid"] != aztables.EDMGUID(guid) {
		t.Fatalf("guid: %#v", e.Properties["Guid"])
	}
	if !time.Time(e.Properties["When"].(aztables.EDMDateTime)).Equal(when) {
		t.Fatalf("datetime: %v want %v", time.Time(e.Properties["When"].(aztables.EDMDateTime)), when)
	}
	if time.Time(e.Timestamp).IsZero() || e.ETag == "" {
		t.Fatalf("timestamp/etag missing: %v %q", e.Timestamp, e.ETag)
	}
}

func TestEntity_AddTwiceIsEntityAlreadyExists409(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "dupe")
	c.AddEntity(ctx, entity("p", "r", nil), nil)
	_, err := c.AddEntity(ctx, entity("p", "r", nil), nil)
	wantCode(t, err, http.StatusConflict, aztables.EntityAlreadyExists)
}

func TestEntity_GetMissingIsResourceNotFound404(t *testing.T) {
	c := newTable(t, "miss")
	_, err := c.GetEntity(context.Background(), "p", "nope", nil)
	wantCode(t, err, http.StatusNotFound, aztables.ResourceNotFound)
}

func TestEntity_KeysWithSpacesPercentAndUnicodeRoundTrip(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "keys")
	for _, k := range [][2]string{{"part one", "row one"}, {"日本", "語"}, {"a+b", "c%20d"}} {
		if _, err := c.AddEntity(ctx, entity(k[0], k[1], map[string]any{"v": "x"}), nil); err != nil {
			t.Fatalf("add %q: %v", k, err)
		}
		e := get(t, c, k[0], k[1])
		if e.PartitionKey != k[0] || e.RowKey != k[1] {
			t.Fatalf("keys %q %q", e.PartitionKey, e.RowKey)
		}
	}
}

func TestEntity_KeysWithForbiddenCharactersAreInvalidInput400(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "badkey")
	for _, k := range [][2]string{{"a#b", "r"}, {"p", "q?x"}, {"a/b", "r"}, {"p", "back\\slash"}} {
		_, err := c.AddEntity(ctx, entity(k[0], k[1], nil), nil)
		wantCode(t, err, http.StatusBadRequest, aztables.InvalidInput)
	}
}

func TestEntity_MergeKeepsOtherPropertiesAndReplaceDropsThem(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "upd")
	c.AddEntity(ctx, entity("p", "m", map[string]any{"a": "1", "b": "2"}), nil)
	c.AddEntity(ctx, entity("p", "r", map[string]any{"a": "1", "b": "2"}), nil)
	if _, err := c.UpdateEntity(ctx, entity("p", "m", map[string]any{"b": "changed", "c": "3"}), &aztables.UpdateEntityOptions{UpdateMode: aztables.UpdateModeMerge}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.UpdateEntity(ctx, entity("p", "r", map[string]any{"b": "changed", "c": "3"}), &aztables.UpdateEntityOptions{UpdateMode: aztables.UpdateModeReplace}); err != nil {
		t.Fatal(err)
	}
	m := get(t, c, "p", "m")
	if m.Properties["a"] != "1" || m.Properties["b"] != "changed" || m.Properties["c"] != "3" {
		t.Fatalf("merged %#v", m.Properties)
	}
	r := get(t, c, "p", "r")
	if _, has := r.Properties["a"]; has || r.Properties["b"] != "changed" {
		t.Fatalf("replaced %#v", r.Properties)
	}
}

func TestEntity_UpdateWithAStaleETagIsUpdateConditionNotSatisfied412(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "etag")
	add, _ := c.AddEntity(ctx, entity("p", "r", map[string]any{"v": "1"}), nil)
	c.UpdateEntity(ctx, entity("p", "r", map[string]any{"v": "2"}), &aztables.UpdateEntityOptions{UpdateMode: aztables.UpdateModeReplace})
	_, err := c.UpdateEntity(ctx, entity("p", "r", map[string]any{"v": "3"}), &aztables.UpdateEntityOptions{IfMatch: &add.ETag, UpdateMode: aztables.UpdateModeReplace})
	wantCode(t, err, http.StatusPreconditionFailed, aztables.UpdateConditionNotSatisfied)
}

func TestEntity_UpsertCreatesThenReplaces(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "ups")
	if _, err := c.UpsertEntity(ctx, entity("p", "r", map[string]any{"v": "1"}), &aztables.UpsertEntityOptions{UpdateMode: aztables.UpdateModeReplace}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.UpsertEntity(ctx, entity("p", "r", map[string]any{"w": "2"}), &aztables.UpsertEntityOptions{UpdateMode: aztables.UpdateModeReplace}); err != nil {
		t.Fatal(err)
	}
	e := get(t, c, "p", "r")
	if _, has := e.Properties["v"]; has || e.Properties["w"] != "2" {
		t.Fatalf("%#v", e.Properties)
	}
}

func TestEntity_DeleteWithETagAndWithoutAndOfAMissingOne(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "rm")
	add, _ := c.AddEntity(ctx, entity("p", "r", nil), nil)
	wrong := azcore.ETag(`W/"datetime'2000-01-01T00%3A00%3A00.0000000Z'"`)
	_, err := c.DeleteEntity(ctx, "p", "r", &aztables.DeleteEntityOptions{IfMatch: &wrong})
	wantCode(t, err, http.StatusPreconditionFailed, aztables.UpdateConditionNotSatisfied)
	if _, err := c.DeleteEntity(ctx, "p", "r", &aztables.DeleteEntityOptions{IfMatch: &add.ETag}); err != nil {
		t.Fatal(err)
	}
	_, err = c.DeleteEntity(ctx, "p", "r", nil)
	wantCode(t, err, http.StatusNotFound, aztables.ResourceNotFound)
}

func seed(t testing.TB, c *aztables.Client) {
	ctx := context.Background()
	for i := 0; i < 30; i++ {
		pk := "even"
		if i%2 == 1 {
			pk = "odd"
		}
		_, err := c.AddEntity(ctx, entity(pk, fmt.Sprintf("r%03d", i), map[string]any{
			"N": int32(i), "Name": fmt.Sprintf("name-%d", i), "Big": aztables.EDMInt64(int64(i) * 1_000_000_000_000), "Even": i%2 == 0,
			"At": aztables.EDMDateTime(time.Date(2024, 1, 1+i, 0, 0, 0, 0, time.UTC)),
		}), nil)
		if err != nil {
			t.Fatal(err)
		}
	}
}

func TestQuery_FilterOnPartitionKeyStringIntBoolInt64DateTimeAndGuid(t *testing.T) {
	c := newTable(t, "flt")
	seed(t, c)
	cases := []struct {
		filter string
		want   int
	}{
		{"PartitionKey eq 'odd'", 15},
		{"N lt 5", 5},
		{"N ge 10 and N lt 20", 10},
		{"Even eq true and N gt 24", 2},
		{"Name eq 'name-7'", 1},
		{"Big gt 20000000000000L", 9},
		{"At ge datetime'2024-01-20T00:00:00Z'", 11},
		{"not (N lt 28)", 2},
		{"PartitionKey eq 'even' or N eq 1", 16},
		{"RowKey ge 'r010' and RowKey le 'r012'", 3},
	}
	for _, tc := range cases {
		f := tc.filter
		got, _ := queryAll(t, c, &aztables.ListEntitiesOptions{Filter: &f})
		if len(got) != tc.want {
			t.Errorf("filter %q: %d entities, want %d", tc.filter, len(got), tc.want)
		}
	}
}

func TestQuery_ResultsAreSortedByPartitionKeyThenRowKey(t *testing.T) {
	c := newTable(t, "ord")
	seed(t, c)
	all, _ := queryAll(t, c, nil)
	if len(all) != 30 {
		t.Fatalf("%d entities", len(all))
	}
	for i := 1; i < len(all); i++ {
		a, b := all[i-1], all[i]
		if a.PartitionKey > b.PartitionKey || a.PartitionKey == b.PartitionKey && a.RowKey >= b.RowKey {
			t.Fatalf("order broken at %d: %s/%s then %s/%s", i, a.PartitionKey, a.RowKey, b.PartitionKey, b.RowKey)
		}
	}
}

func TestQuery_TopPagesWithContinuationTokens(t *testing.T) {
	c := newTable(t, "page")
	seed(t, c)
	all, pages := queryAll(t, c, &aztables.ListEntitiesOptions{Top: ptr(int32(7))})
	if len(all) != 30 || pages != 5 {
		t.Fatalf("entities %d pages %d", len(all), pages)
	}
	seen := map[string]bool{}
	for _, e := range all {
		k := e.PartitionKey + "/" + e.RowKey
		if seen[k] {
			t.Fatalf("duplicate %s across pages", k)
		}
		seen[k] = true
	}
}

func TestQuery_SelectReturnsOnlyTheRequestedProperties(t *testing.T) {
	c := newTable(t, "sel")
	seed(t, c)
	sel := "Name"
	got, _ := queryAll(t, c, &aztables.ListEntitiesOptions{Select: &sel})
	if len(got) != 30 {
		t.Fatalf("%d", len(got))
	}
	for _, e := range got {
		if _, has := e.Properties["Name"]; !has {
			t.Fatalf("Name missing: %#v", e.Properties)
		}
		if _, has := e.Properties["N"]; has {
			t.Fatal("N was not selected")
		}
	}
}

func TestQuery_FilterWithQuoteInsideAString(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "quote")
	c.AddEntity(ctx, entity("p", "1", map[string]any{"Name": "o'brien"}), nil)
	c.AddEntity(ctx, entity("p", "2", map[string]any{"Name": "other"}), nil)
	f := "Name eq 'o''brien'"
	got, _ := queryAll(t, c, &aztables.ListEntitiesOptions{Filter: &f})
	if len(got) != 1 || got[0].RowKey != "1" {
		t.Fatalf("%v", got)
	}
}

func TestTransaction_AddUpdateDeleteInOnePartitionAreAtomic(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "tx")
	c.AddEntity(ctx, entity("p", "old", map[string]any{"v": "x"}), nil)
	c.AddEntity(ctx, entity("p", "gone", map[string]any{"v": "x"}), nil)
	actions := []aztables.TransactionAction{
		{ActionType: aztables.TransactionTypeAdd, Entity: entity("p", "new1", map[string]any{"v": "1"})},
		{ActionType: aztables.TransactionTypeAdd, Entity: entity("p", "new2", map[string]any{"v": "2"})},
		{ActionType: aztables.TransactionTypeUpdateMerge, Entity: entity("p", "old", map[string]any{"w": "y"})},
		{ActionType: aztables.TransactionTypeDelete, Entity: entity("p", "gone", nil)},
	}
	if _, err := c.SubmitTransaction(ctx, actions, nil); err != nil {
		t.Fatal(err)
	}
	all, _ := queryAll(t, c, nil)
	if len(all) != 3 {
		t.Fatalf("%d entities after the transaction", len(all))
	}
	if e := get(t, c, "p", "old"); e.Properties["v"] != "x" || e.Properties["w"] != "y" {
		t.Fatalf("merge in transaction: %#v", e.Properties)
	}
}

func TestTransaction_OneFailingActionRollsBackTheWholeBatch(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "txfail")
	c.AddEntity(ctx, entity("p", "exists", nil), nil)
	actions := []aztables.TransactionAction{
		{ActionType: aztables.TransactionTypeAdd, Entity: entity("p", "fresh", nil)},
		{ActionType: aztables.TransactionTypeAdd, Entity: entity("p", "exists", nil)}, // conflicts
	}
	_, err := c.SubmitTransaction(ctx, actions, nil)
	if err == nil {
		t.Fatal("transaction with a conflicting add must fail")
	}
	// The SDK has no transaction error type: it reads the inner responses and returns a ResponseError when one is 4xx.
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if _, err := c.GetEntity(ctx, "p", "fresh", nil); err == nil {
		t.Fatal("first add should have been rolled back")
	}
}

func TestTransaction_EntitiesInDifferentPartitionsAreRefused400(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "txpart")
	_, err := c.SubmitTransaction(ctx, []aztables.TransactionAction{
		{ActionType: aztables.TransactionTypeAdd, Entity: entity("p1", "a", nil)},
		{ActionType: aztables.TransactionTypeAdd, Entity: entity("p2", "b", nil)},
	}, nil)
	if err == nil {
		t.Fatal("a transaction across partitions must fail")
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("unexpected error kind: %v", err)
	}
	// nothing may have been written
	if all, _ := queryAll(t, c, nil); len(all) != 0 {
		t.Fatalf("%d entities written by a refused transaction", len(all))
	}
}

func TestSAS_TableSasWithQueryPermissionReadsButCannotAdd(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "sas")
	c.AddEntity(ctx, entity("p", "r", map[string]any{"v": "1"}), nil)
	u, err := c.GetTableSASURL(aztables.SASPermissions{Read: true}, time.Now().UTC().Add(-time.Minute), time.Now().UTC().Add(time.Hour))
	if err != nil {
		t.Fatal(err)
	}
	cs, err := aztables.NewClientWithNoCredential(u, nil)
	if err != nil {
		t.Fatal(err)
	}
	if e := get(t, cs, "p", "r"); e.Properties["v"] != "1" {
		t.Fatalf("%#v", e.Properties)
	}
	_, err = cs.AddEntity(ctx, entity("p", "new", nil), nil)
	if err == nil {
		t.Fatal("add without Add permission must fail")
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) || re.StatusCode != http.StatusForbidden {
		t.Fatalf("add with read-only SAS: %v", err)
	}
}

func TestSAS_PartitionKeyRangeLimitsWhatTheSasCanRead(t *testing.T) {
	ctx := context.Background()
	c := newTable(t, "sasr")
	c.AddEntity(ctx, entity("a", "1", nil), nil)
	c.AddEntity(ctx, entity("b", "1", nil), nil)
	q, err := aztables.SASSignatureValues{Protocol: aztables.SASProtocolHTTPSandHTTP, StartTime: time.Now().UTC().Add(-time.Minute), ExpiryTime: time.Now().UTC().Add(time.Hour),
		Permissions: (&aztables.SASPermissions{Read: true}).String(), TableName: tableName(c),
		StartPartitionKey: "a", EndPartitionKey: "a"}.Sign(cred(t))
	if err != nil {
		t.Fatal(err)
	}
	// SDK limit: Sign() signs the partition key range but leaves spk/epk out of the query string it returns
	// (aztables 1.4.1, sas_query_params.go has no way to set them), so add them by hand.
	q += "&spk=a&epk=a"
	cs, _ := aztables.NewClientWithNoCredential(tableURL(c)+"?"+q, nil)
	if _, err := cs.GetEntity(ctx, "a", "1", nil); err != nil {
		t.Fatalf("inside the range: %v", err)
	}
	_, err = cs.GetEntity(ctx, "b", "1", nil)
	if err == nil {
		t.Fatal("entity outside the partition key range was readable")
	}
}

var names sync.Map // *aztables.Client -> table name

func tableName(c *aztables.Client) string { n, _ := names.Load(c); return n.(string) }

func tableURL(c *aztables.Client) string {
	return emu.E.HTTP("table", emu.DevAccount) + "/" + tableName(c)
}

func TestSharedKey_WrongKeyIsAuthenticationFailed403(t *testing.T) {
	bad, err := aztables.NewSharedKeyCredential(emu.DevAccount, "eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eA==")
	if err != nil {
		t.Fatal(err)
	}
	s, _ := aztables.NewServiceClientWithSharedKey(emu.E.HTTP("table", emu.DevAccount), bad, nil)
	_, err = s.CreateTable(context.Background(), emu.Name("badkey"), nil)
	wantCode(t, err, http.StatusForbidden, aztables.TableErrorCode("AuthenticationFailed"))
}
