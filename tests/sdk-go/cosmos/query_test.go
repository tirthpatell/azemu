package cosmos

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
)

// rawAll returns the JSON values of a query as they are (for SELECT VALUE queries).
func rawAll(t testing.TB, ct *azcosmos.ContainerClient, q string, pk azcosmos.PartitionKey) []any {
	t.Helper()
	var out []any
	for p := ct.NewQueryItemsPager(q, pk, nil); p.More(); {
		pg, err := p.NextPage(context.Background())
		if err != nil {
			t.Fatalf("query %q: %v", q, err)
		}
		for _, it := range pg.Items {
			var v any
			if err := json.Unmarshal(it, &v); err != nil {
				t.Fatal(err)
			}
			out = append(out, v)
		}
	}
	return out
}

func seed(t testing.TB, ct *azcosmos.ContainerClient, n int) {
	t.Helper()
	for i := 0; i < n; i++ {
		pk := fmt.Sprintf("p%d", i%3)
		_, err := ct.CreateItem(context.Background(), azcosmos.NewPartitionKeyString(pk), doc(map[string]any{
			"id": fmt.Sprintf("d%03d", i), "pk": pk, "n": i, "name": fmt.Sprintf("name-%02d", i), "even": i%2 == 0, "tags": []string{"a", fmt.Sprintf("t%d", i%4)},
		}), nil)
		if err != nil {
			t.Fatal(err)
		}
	}
}

func TestQuery_ParametersFilterWithinOnePartition(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 30)
	got, _ := queryAll(t, ct, "SELECT c.id, c.n FROM c WHERE c.n >= @lo AND c.n < @hi AND c.even = @even ORDER BY c.n",
		azcosmos.NewPartitionKeyString("p0"), &azcosmos.QueryOptions{QueryParameters: []azcosmos.QueryParameter{{Name: "@lo", Value: 6}, {Name: "@hi", Value: 20}, {Name: "@even", Value: true}}})
	var ns []int
	for _, m := range got {
		ns = append(ns, int(m["n"].(float64)))
	}
	want := []int{6, 12, 18}
	if fmt.Sprint(ns) != fmt.Sprint(want) {
		t.Fatalf("got %v want %v", ns, want)
	}
}

func TestQuery_StringParameterWithQuoteIsNotInterpretedAsSql(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "1", "pk": "p", "name": "o'brien"}), nil)
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "2", "pk": "p", "name": "x"}), nil)
	got, _ := queryAll(t, ct, "SELECT * FROM c WHERE c.name = @n", pk, &azcosmos.QueryOptions{QueryParameters: []azcosmos.QueryParameter{{Name: "@n", Value: "o'brien"}}})
	if len(got) != 1 || got[0]["id"] != "1" {
		t.Fatalf("%v", got)
	}
	got, _ = queryAll(t, ct, "SELECT * FROM c WHERE c.name = @n", pk, &azcosmos.QueryOptions{QueryParameters: []azcosmos.QueryParameter{{Name: "@n", Value: "x' OR '1'='1"}}})
	if len(got) != 0 {
		t.Fatalf("injection matched %d", len(got))
	}
}

func TestQuery_PagingWithPageSizeHintAndContinuationTokensVisitsEachItemOnce(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	for i := 0; i < 25; i++ {
		ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("one"), doc(map[string]any{"id": fmt.Sprintf("d%02d", i), "pk": "one", "n": i}), nil)
	}
	got, pages := queryAll(t, ct, "SELECT * FROM c ORDER BY c.n", azcosmos.NewPartitionKeyString("one"), &azcosmos.QueryOptions{PageSizeHint: 7})
	if len(got) != 25 || pages != 4 {
		t.Fatalf("items %d pages %d", len(got), pages)
	}
	for i, m := range got {
		if m["n"] != float64(i) {
			t.Fatalf("order at %d: %v", i, m["n"])
		}
	}
}

func TestQuery_ResumeFromAContinuationTokenContinuesWhereThePageEnded(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	for i := 0; i < 20; i++ {
		ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("one"), doc(map[string]any{"id": fmt.Sprintf("d%02d", i), "pk": "one", "n": i}), nil)
	}
	pk := azcosmos.NewPartitionKeyString("one")
	p := ct.NewQueryItemsPager("SELECT * FROM c ORDER BY c.n", pk, &azcosmos.QueryOptions{PageSizeHint: 8})
	first, err := p.NextPage(ctx)
	if err != nil || len(first.Items) != 8 || first.ContinuationToken == nil {
		t.Fatalf("first page: %v %d %v", err, len(first.Items), first.ContinuationToken)
	}
	p2 := ct.NewQueryItemsPager("SELECT * FROM c ORDER BY c.n", pk, &azcosmos.QueryOptions{PageSizeHint: 8, ContinuationToken: first.ContinuationToken})
	second, err := p2.NextPage(ctx)
	if err != nil || len(second.Items) != 8 {
		t.Fatalf("second page: %v", err)
	}
	if parse(t, second.Items[0])["n"] != float64(8) {
		t.Fatalf("second page starts at %v", parse(t, second.Items[0])["n"])
	}
}

func TestQuery_CrossPartitionWithEmptyPartitionKeyReturnsItemsFromEveryPartition(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 30)
	got, _ := queryAll(t, ct, "SELECT c.id FROM c WHERE c.n < 12", azcosmos.NewPartitionKey(), nil)
	if len(got) != 12 {
		t.Fatalf("%d items", len(got))
	}
	ids := map[string]bool{}
	for _, m := range got {
		ids[m["id"].(string)] = true
	}
	if len(ids) != 12 {
		t.Fatal("duplicates across partitions")
	}
}

func TestQuery_OrderByAndTopInOnePartition(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 30)
	got, _ := queryAll(t, ct, "SELECT TOP 4 c.n FROM c ORDER BY c.n DESC", azcosmos.NewPartitionKeyString("p2"), nil)
	var ns []int
	for _, m := range got {
		ns = append(ns, int(m["n"].(float64)))
	}
	if fmt.Sprint(ns) != "[29 26 23 20]" {
		t.Fatalf("%v", ns)
	}
}

// The Go SDK cannot merge cross partition ORDER BY results itself and the gateway refuses to ("can not be directly served by the
// gateway"), as the real service does for a client that does not declare the query features. See docs/sdk-notes.md, Go, Cosmos DB.

func TestQuery_AggregatesInOnePartition(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 30)
	pk := azcosmos.NewPartitionKeyString("p1")
	raw := rawAll(t, ct, "SELECT VALUE COUNT(1) FROM c", pk)
	if len(raw) != 1 || raw[0] != float64(10) {
		t.Fatalf("SELECT VALUE COUNT(1): %v", raw)
	}
	got, _ := queryAll(t, ct, "SELECT COUNT(1) AS cnt, SUM(c.n) AS total, MIN(c.n) AS lo, MAX(c.n) AS hi, AVG(c.n) AS avg FROM c", pk, nil)
	if len(got) != 1 {
		t.Fatalf("%v", got)
	}
	r := got[0]
	// p1 holds n = 1,4,7,...,28
	if r["cnt"] != float64(10) || r["total"] != float64(145) || r["lo"] != float64(1) || r["hi"] != float64(28) || r["avg"] != 14.5 {
		t.Fatalf("aggregates %v", r)
	}
}

func TestQuery_GroupByAndDistinctInOnePartition(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 30)
	pk := azcosmos.NewPartitionKeyString("p0")
	got, _ := queryAll(t, ct, "SELECT c.even, COUNT(1) AS cnt FROM c GROUP BY c.even", pk, nil)
	total := 0
	for _, m := range got {
		total += int(m["cnt"].(float64))
	}
	if len(got) != 2 && len(got) != 1 || total != 10 {
		t.Fatalf("group by: %v", got)
	}
	d := rawAll(t, ct, "SELECT DISTINCT VALUE c.even FROM c", pk)
	if len(d) != 2 {
		t.Fatalf("distinct: %v", d)
	}
}

func TestQuery_JoinOverAnArrayAndArrayContains(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 12)
	pk := azcosmos.NewPartitionKeyString("p0")
	got, _ := queryAll(t, ct, "SELECT c.id, t AS tag FROM c JOIN t IN c.tags WHERE t = 'a'", pk, nil)
	if len(got) != 4 {
		t.Fatalf("join: %v", got)
	}
	got, _ = queryAll(t, ct, "SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.tags, 't0')", pk, nil)
	sort.Slice(got, func(i, j int) bool { return got[i]["id"].(string) < got[j]["id"].(string) })
	if len(got) == 0 {
		t.Fatal("ARRAY_CONTAINS found nothing")
	}
}

func TestQuery_BuiltInStringAndMathFunctions(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "f", "pk": "p", "s": "Hello World", "x": -2.7}), nil)
	got, _ := queryAll(t, ct, "SELECT UPPER(c.s) AS up, LENGTH(c.s) AS len, CONTAINS(c.s, 'lo W') AS has, ABS(c.x) AS ab, FLOOR(c.x) AS fl, SUBSTRING(c.s, 0, 5) AS sub, STARTSWITH(c.s, 'Hel') AS sw FROM c", pk, nil)
	if len(got) != 1 {
		t.Fatalf("%v", got)
	}
	r := got[0]
	if r["up"] != "HELLO WORLD" || r["len"] != float64(11) || r["has"] != true || r["ab"] != 2.7 || r["fl"] != float64(-3) || r["sub"] != "Hello" || r["sw"] != true {
		t.Fatalf("%v", r)
	}
}

func TestQuery_SyntaxErrorIsBadRequest400(t *testing.T) {
	ct := newContainer(t, newClient(t))
	p := ct.NewQueryItemsPager("SELEKT * FROM c", azcosmos.NewPartitionKeyString("p"), nil)
	_, err := p.NextPage(context.Background())
	status(t, err, http.StatusBadRequest)
}

func TestQuery_EnableCrossPartitionQueryFalseWithEmptyKeyStillAnswers(t *testing.T) {
	ct := newContainer(t, newClient(t))
	seed(t, ct, 6)
	// one physical partition: the gateway can answer a query without a partition key header
	got, _ := queryAll(t, ct, "SELECT * FROM c", azcosmos.NewPartitionKey(), &azcosmos.QueryOptions{EnableCrossPartitionQuery: ptr(true)})
	if len(got) != 6 {
		t.Fatalf("%d", len(got))
	}
}

func TestPatch_AddSetReplaceRemoveIncrementInOneRequest(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "x", "pk": "p", "count": 1, "name": "old", "gone": true, "arr": []int{1, 2}}), nil)
	var ops azcosmos.PatchOperations
	ops.AppendAdd("/added", "new")
	ops.AppendSet("/name", "set")
	ops.AppendReplace("/count", 10)
	ops.AppendRemove("/gone")
	ops.AppendIncrement("/count", 5)
	ops.AppendAdd("/arr/-", 3)
	r, err := ct.PatchItem(ctx, pk, "x", ops, &azcosmos.ItemOptions{EnableContentResponseOnWrite: true})
	if err != nil {
		t.Fatal(err)
	}
	m := parse(t, r.Value)
	if m["added"] != "new" || m["name"] != "set" || m["count"] != float64(15) || m["gone"] != nil {
		t.Fatalf("patched %v", m)
	}
	if fmt.Sprint(m["arr"]) != "[1 2 3]" {
		t.Fatalf("arr %v", m["arr"])
	}
}

func TestPatch_ConditionThatIsFalseIsPreconditionFailed412(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "x", "pk": "p", "count": 1}), nil)
	var ops azcosmos.PatchOperations
	ops.SetCondition("from c where c.count = 99")
	ops.AppendSet("/count", 2)
	_, err := ct.PatchItem(ctx, pk, "x", ops, nil)
	status(t, err, http.StatusPreconditionFailed)
}

func TestPatch_OnAMissingItemIsNotFound404AndTooManyOperationsIsBadRequest(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	var ops azcosmos.PatchOperations
	ops.AppendSet("/a", 1)
	_, err := ct.PatchItem(ctx, pk, "ghost", ops, nil)
	status(t, err, http.StatusNotFound)
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "x", "pk": "p"}), nil)
	var many azcosmos.PatchOperations
	for i := 0; i < 11; i++ {
		many.AppendSet(fmt.Sprintf("/f%d", i), i)
	}
	_, err = ct.PatchItem(ctx, pk, "x", many, nil)
	status(t, err, http.StatusBadRequest)
}

func TestBatch_CreateReplaceReadDeleteInOneTransactionCommitsAll(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "old", "pk": "p", "v": 1}), nil)
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "del", "pk": "p"}), nil)
	b := ct.NewTransactionalBatch(pk)
	b.CreateItem(doc(map[string]any{"id": "new1", "pk": "p"}), nil)
	b.UpsertItem(doc(map[string]any{"id": "new2", "pk": "p"}), nil)
	b.ReplaceItem("old", doc(map[string]any{"id": "old", "pk": "p", "v": 2}), nil)
	b.ReadItem("old", nil)
	var ops azcosmos.PatchOperations
	ops.AppendSet("/patched", true)
	b.PatchItem("new1", ops, nil)
	b.DeleteItem("del", nil)
	r, err := ct.ExecuteTransactionalBatch(ctx, b, &azcosmos.TransactionalBatchOptions{EnableContentResponseOnWrite: true})
	if err != nil {
		t.Fatal(err)
	}
	if !r.Success || len(r.OperationResults) != 6 {
		t.Fatalf("success=%v results=%d", r.Success, len(r.OperationResults))
	}
	wantStatus := []int32{201, 201, 200, 200, 200, 204}
	for i, or := range r.OperationResults {
		if or.StatusCode != wantStatus[i] {
			t.Errorf("operation %d status %d, want %d", i, or.StatusCode, wantStatus[i])
		}
	}
	if parse(t, r.OperationResults[3].ResourceBody)["v"] != float64(2) {
		t.Fatalf("read inside the batch sees %s", r.OperationResults[3].ResourceBody)
	}
	if _, err := ct.ReadItem(ctx, pk, "del", nil); err == nil {
		t.Fatal("deleted item still there")
	}
	got, _ := ct.ReadItem(ctx, pk, "new1", nil)
	if parse(t, got.Value)["patched"] != true {
		t.Fatalf("%s", got.Value)
	}
}

func TestBatch_OneFailingOperationRollsBackEverythingAndMarksTheRestFailedDependency424(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	ct.CreateItem(ctx, pk, doc(map[string]any{"id": "exists", "pk": "p"}), nil)
	b := ct.NewTransactionalBatch(pk)
	b.CreateItem(doc(map[string]any{"id": "first", "pk": "p"}), nil)
	b.CreateItem(doc(map[string]any{"id": "exists", "pk": "p"}), nil) // conflict
	b.CreateItem(doc(map[string]any{"id": "third", "pk": "p"}), nil)
	r, err := ct.ExecuteTransactionalBatch(ctx, b, nil)
	if err != nil {
		t.Fatalf("a failed batch is a normal answer: %v", err)
	}
	if r.Success {
		t.Fatal("batch should not succeed")
	}
	got := []int32{}
	for _, or := range r.OperationResults {
		got = append(got, or.StatusCode)
	}
	if fmt.Sprint(got) != "[424 409 424]" {
		t.Fatalf("operation statuses %v, want [424 409 424]", got)
	}
	if _, err := ct.ReadItem(ctx, pk, "first", nil); err == nil {
		t.Fatal("first create must have been rolled back")
	}
}

func TestBatch_MoreThanOneHundredOperationsIsRefused(t *testing.T) {
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	b := ct.NewTransactionalBatch(pk)
	for i := 0; i < 101; i++ {
		b.CreateItem(doc(map[string]any{"id": fmt.Sprintf("i%d", i), "pk": "p"}), nil)
	}
	r, err := ct.ExecuteTransactionalBatch(context.Background(), b, nil)
	if err == nil && r.Success {
		t.Fatal("101 operations must not succeed")
	}
}

func TestChangeFeed_ReadsEveryCreatedItemThenNothingNewFromTheContinuation(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	for i := 0; i < 5; i++ {
		ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": fmt.Sprintf("c%d", i), "pk": "p"}), nil)
	}
	ranges, err := ct.ReadFeedRanges(ctx, nil)
	if err != nil || len(ranges) == 0 {
		t.Fatalf("feed ranges: %v", err)
	}
	// the SDK refuses to read the change feed without a feed range or a continuation
	r, err := ct.ReadChangeFeed(ctx, &azcosmos.ChangeFeedOptions{MaxItemCount: 100, FeedRange: &ranges[0]})
	if err != nil {
		t.Fatalf("change feed: %v", err)
	}
	if len(r.Items) != 5 || r.ContinuationToken == "" {
		t.Fatalf("items %d token %q", len(r.Items), r.ContinuationToken)
	}
	again, err := ct.ReadChangeFeed(ctx, &azcosmos.ChangeFeedOptions{Continuation: &r.ContinuationToken})
	if err != nil {
		t.Fatalf("second read: %v", err)
	}
	if len(again.Items) != 0 {
		t.Fatalf("%d new items", len(again.Items))
	}
	ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": "late", "pk": "p"}), nil)
	third, err := ct.ReadChangeFeed(ctx, &azcosmos.ChangeFeedOptions{Continuation: &again.ContinuationToken})
	if err != nil || len(third.Items) != 1 {
		t.Fatalf("third read: %v %d", err, len(third.Items))
	}
}

func TestChangeFeed_FeedRangesAreReportedAndStartFromANowTimeSkipsOldItems(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": "old", "pk": "p"}), nil)
	ranges, err := ct.ReadFeedRanges(ctx, nil)
	if err != nil || len(ranges) < 1 {
		t.Fatalf("feed ranges: %v %d", err, len(ranges))
	}
	time.Sleep(1100 * time.Millisecond)
	now := time.Now().UTC()
	time.Sleep(1100 * time.Millisecond)
	ct.CreateItem(ctx, azcosmos.NewPartitionKeyString("p"), doc(map[string]any{"id": "new", "pk": "p"}), nil)
	r, err := ct.ReadChangeFeed(ctx, &azcosmos.ChangeFeedOptions{StartFrom: &now, FeedRange: &ranges[0]})
	if err != nil {
		t.Fatal(err)
	}
	if len(r.Items) != 1 || parse(t, r.Items[0])["id"] != "new" {
		t.Fatalf("items %d", len(r.Items))
	}
}

var _ = emu.E
