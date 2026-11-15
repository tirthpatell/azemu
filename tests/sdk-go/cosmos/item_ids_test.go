package cosmos

// S10. The Go SDK does not look at an item id before it sends it (the Python, JavaScript and Java SDKs refuse
// some characters themselves), so these tests reach the service. What a real account answered in November 2026: an item id
// may hold any character, at most 1,023 characters (a longer one is a 400; one with more than 1,023 bytes in UTF-8 is a 500);
// an id with '#', '/', a backslash, a trailing space or a percent escape in it is created but cannot be reached by id (401).
// A partition key value is at most 2,048 bytes ("Service quotas and default limits").

import (
	"context"
	"net/http"
	"strings"
	"testing"

	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
)

var awkwardIDs = []string{"a?b=c", "?", "100% sure", "naïve ☃ id"}

// created, but a request by id never reaches them
var unreachableIDs = []string{"USER#123", "#", "a#b?c#d", "p%20q", "x "}

func TestItemIDs_QuestionMarkAndPercentWorkInEveryItemOperation(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	for _, id := range awkwardIDs {
		if _, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": id, "pk": "p", "v": 1}), nil); err != nil {
			t.Fatalf("create %q: %v", id, err)
		}
		r, err := ct.ReadItem(ctx, pk, id, nil)
		if err != nil {
			t.Fatalf("read %q: %v", id, err)
		}
		m := parse(t, r.Value)
		if m["id"] != id || m["_self"] == nil || strings.ContainsAny(m["_self"].(string), "#?") {
			t.Fatalf("%q came back as %v", id, m)
		}
		if _, err := ct.ReplaceItem(ctx, pk, id, doc(map[string]any{"id": id, "pk": "p", "v": 2}), nil); err != nil {
			t.Fatalf("replace %q: %v", id, err)
		}
		var ops azcosmos.PatchOperations
		ops.AppendIncrement("/v", 5)
		p, err := ct.PatchItem(ctx, pk, id, ops, &azcosmos.ItemOptions{EnableContentResponseOnWrite: true})
		if err != nil {
			t.Fatalf("patch %q: %v", id, err)
		}
		if parse(t, p.Value)["v"] != float64(7) {
			t.Fatalf("patch %q gave %s", id, p.Value)
		}
		if _, err := ct.UpsertItem(ctx, pk, doc(map[string]any{"id": id, "pk": "p", "v": 9}), nil); err != nil {
			t.Fatalf("upsert %q: %v", id, err)
		}
		_, err = ct.CreateItem(ctx, pk, doc(map[string]any{"id": id, "pk": "p"}), nil)
		status(t, err, http.StatusConflict)
	}
	for _, id := range awkwardIDs {
		if _, err := ct.DeleteItem(ctx, pk, id, nil); err != nil {
			t.Fatalf("delete %q: %v", id, err)
		}
		_, err := ct.ReadItem(ctx, pk, id, nil)
		status(t, err, http.StatusNotFound)
	}
}

func TestItemIDs_AnIdTheGatewayCannotAddressIsCreatedAndEveryRequestByIdIs401(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	for _, id := range unreachableIDs {
		if _, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": id, "pk": "p", "v": 1}), nil); err != nil {
			t.Fatalf("create %q: %v", id, err)
		}
		_, err := ct.ReadItem(ctx, pk, id, nil)
		status(t, err, http.StatusUnauthorized)
		_, err = ct.ReplaceItem(ctx, pk, id, doc(map[string]any{"id": id, "pk": "p", "v": 2}), nil)
		status(t, err, http.StatusUnauthorized)
		_, err = ct.DeleteItem(ctx, pk, id, nil)
		status(t, err, http.StatusUnauthorized)
	}
	rows, _ := queryAll(t, ct, "SELECT c.id FROM c WHERE c.id = 'USER#123'", pk, nil)
	if len(rows) != 1 || rows[0]["id"] != "USER#123" {
		t.Fatalf("query: %v", rows)
	}
}

func TestItemIDs_ABatchTakesIdsWithHashAndQuestionMark(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	b := ct.NewTransactionalBatch(pk)
	b.CreateItem(doc(map[string]any{"id": "B#1", "pk": "p"}), nil)
	b.CreateItem(doc(map[string]any{"id": "B?2", "pk": "p"}), nil)
	b.ReadItem("B#1", nil)
	var ops azcosmos.PatchOperations
	ops.AppendSet("/x", 1)
	b.PatchItem("B?2", ops, nil)
	b.ReplaceItem("B#1", doc(map[string]any{"id": "B#1", "pk": "p", "n": 2}), nil)
	b.DeleteItem("B?2", nil)
	r, err := ct.ExecuteTransactionalBatch(ctx, b, &azcosmos.TransactionalBatchOptions{EnableContentResponseOnWrite: true})
	if err != nil {
		t.Fatal(err)
	}
	if !r.Success || len(r.OperationResults) != 6 {
		t.Fatalf("success=%v results=%d", r.Success, len(r.OperationResults))
	}
	if parse(t, r.OperationResults[2].ResourceBody)["id"] != "B#1" {
		t.Fatalf("read in the batch: %s", r.OperationResults[2].ResourceBody)
	}
	// a request by id cannot reach an id with '#'
	_, err = ct.ReadItem(ctx, pk, "B#1", nil)
	status(t, err, http.StatusUnauthorized)
}

func TestItemIDs_AnIdIsAcceptedUpTo1023BytesAndRefusedBeyond(t *testing.T) {
	// 1,024 characters is a 400; 342 characters of 3 bytes (1,026 bytes) is a 500 on Azure
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	pk := azcosmos.NewPartitionKeyString("p")
	for _, id := range []string{strings.Repeat("a", 256), strings.Repeat("a", 1023), strings.Repeat("€", 341)} {
		if _, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": id, "pk": "p"}), nil); err != nil {
			t.Fatalf("create an id of %d bytes: %v", len(id), err)
		}
		if _, err := ct.ReadItem(ctx, pk, id, nil); err != nil {
			t.Fatalf("read an id of %d bytes: %v", len(id), err)
		}
	}
	_, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": strings.Repeat("a", 1024), "pk": "p"}), nil)
	status(t, err, http.StatusBadRequest)
	_, err = ct.CreateItem(ctx, pk, doc(map[string]any{"id": strings.Repeat("€", 342), "pk": "p"}), nil)
	status(t, err, http.StatusInternalServerError)
}

func TestItemIDs_APartitionKeyValueIsAcceptedUpTo2048BytesAndRefusedBeyond(t *testing.T) {
	ctx := context.Background()
	ct := newContainer(t, newClient(t))
	ok := strings.Repeat("k", 2048)
	pk := azcosmos.NewPartitionKeyString(ok)
	if _, err := ct.CreateItem(ctx, pk, doc(map[string]any{"id": "1", "pk": ok}), nil); err != nil {
		t.Fatalf("create with 2048 bytes: %v", err)
	}
	if _, err := ct.ReadItem(ctx, pk, "1", nil); err != nil {
		t.Fatalf("read with 2048 bytes: %v", err)
	}
	long := strings.Repeat("k", 2049)
	_, err := ct.CreateItem(ctx, azcosmos.NewPartitionKeyString(long), doc(map[string]any{"id": "2", "pk": long}), nil)
	status(t, err, http.StatusBadRequest)
}
