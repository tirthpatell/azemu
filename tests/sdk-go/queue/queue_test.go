package queue

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azqueue"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azqueue/queueerror"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azqueue/sas"
)

func TestMain(m *testing.M) { emu.Main(m) }

func cred(t testing.TB) *azqueue.SharedKeyCredential {
	t.Helper()
	c, err := azqueue.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func svc(t testing.TB) *azqueue.ServiceClient {
	t.Helper()
	s, err := azqueue.NewServiceClientWithSharedKeyCredential(emu.E.HTTP("queue", emu.DevAccount), cred(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func newQueue(t testing.TB, prefix string) *azqueue.QueueClient {
	t.Helper()
	s := svc(t)
	name := emu.Name(prefix)
	if _, err := s.CreateQueue(context.Background(), name, nil); err != nil {
		t.Fatalf("create queue: %v", err)
	}
	return s.NewQueueClient(name)
}

func wantCode(t testing.TB, err error, status int, code queueerror.Code) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected %d %s, got success", status, code)
	}
	var re *azcore.ResponseError
	if !errors.As(err, &re) {
		t.Fatalf("not a ResponseError: %v", err)
	}
	if re.StatusCode != status || !queueerror.HasCode(err, code) {
		t.Fatalf("got %d %q, want %d %q", re.StatusCode, re.ErrorCode, status, code)
	}
}

func ptr[T any](v T) *T { return &v }

func TestQueue_CreateTwiceWithSameMetadataIsIdempotentAndDifferentMetadataIsQueueAlreadyExists409(t *testing.T) {
	ctx := context.Background()
	s := svc(t)
	name := emu.Name("idem")
	meta := map[string]*string{"a": ptr("1")}
	if _, err := s.CreateQueue(ctx, name, &azqueue.CreateOptions{Metadata: meta}); err != nil {
		t.Fatal(err)
	}
	if _, err := s.CreateQueue(ctx, name, &azqueue.CreateOptions{Metadata: meta}); err != nil {
		t.Fatalf("second create with same metadata: %v", err)
	}
	_, err := s.CreateQueue(ctx, name, &azqueue.CreateOptions{Metadata: map[string]*string{"a": ptr("2")}})
	wantCode(t, err, http.StatusConflict, queueerror.QueueAlreadyExists)
}

func TestQueue_EnqueueDequeueDeleteRoundTrip(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "rt")
	enq, err := q.EnqueueMessage(ctx, "hello queue", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(enq.Messages) != 1 || enq.Messages[0].MessageID == nil || enq.Messages[0].PopReceipt == nil {
		t.Fatalf("enqueue response %+v", enq.Messages)
	}
	deq, err := q.DequeueMessage(ctx, nil)
	if err != nil || len(deq.Messages) != 1 {
		t.Fatalf("dequeue: %v %d", err, len(deq.Messages))
	}
	m := deq.Messages[0]
	if *m.MessageText != "hello queue" || *m.DequeueCount != 1 || *m.MessageID != *enq.Messages[0].MessageID {
		t.Fatalf("message %+v", m)
	}
	if _, err := q.DeleteMessage(ctx, *m.MessageID, *m.PopReceipt, nil); err != nil {
		t.Fatal(err)
	}
	again, _ := q.DequeueMessage(ctx, nil)
	if len(again.Messages) != 0 {
		t.Fatal("deleted message came back")
	}
}

func TestQueue_DequeuedMessageIsInvisibleUntilTheVisibilityTimeoutPasses(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "vis")
	q.EnqueueMessage(ctx, "m", nil)
	first, _ := q.DequeueMessage(ctx, &azqueue.DequeueMessageOptions{VisibilityTimeout: ptr(int32(2))})
	if len(first.Messages) != 1 {
		t.Fatal("no message")
	}
	none, _ := q.DequeueMessage(ctx, nil)
	if len(none.Messages) != 0 {
		t.Fatal("message visible while invisible")
	}
	time.Sleep(2200 * time.Millisecond)
	second, err := q.DequeueMessage(ctx, nil)
	if err != nil || len(second.Messages) != 1 {
		t.Fatalf("after timeout: %v %d", err, len(second.Messages))
	}
	if *second.Messages[0].DequeueCount != 2 {
		t.Fatalf("dequeue count %d", *second.Messages[0].DequeueCount)
	}
}

func TestQueue_DeleteWithAStalePopReceiptIsPopReceiptMismatch400(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "stale")
	q.EnqueueMessage(ctx, "m", nil)
	d1, _ := q.DequeueMessage(ctx, &azqueue.DequeueMessageOptions{VisibilityTimeout: ptr(int32(1))})
	time.Sleep(1200 * time.Millisecond)
	q.DequeueMessage(ctx, nil)
	m := d1.Messages[0]
	_, err := q.DeleteMessage(ctx, *m.MessageID, *m.PopReceipt, nil)
	wantCode(t, err, http.StatusBadRequest, queueerror.Code("PopReceiptMismatch"))
}

func TestQueue_DeleteOfAnUnknownMessageIdIsMessageNotFound404(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "nomsg")
	enq, _ := q.EnqueueMessage(ctx, "m", nil)
	_, err := q.DeleteMessage(ctx, "11111111-2222-3333-4444-555555555555", *enq.Messages[0].PopReceipt, nil)
	wantCode(t, err, http.StatusNotFound, queueerror.MessageNotFound)
}

func TestQueue_PeekDoesNotChangeVisibilityOrDequeueCount(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "peek")
	for i := 0; i < 3; i++ {
		q.EnqueueMessage(ctx, fmt.Sprintf("m%d", i), nil)
	}
	p, err := q.PeekMessages(ctx, &azqueue.PeekMessagesOptions{NumberOfMessages: ptr(int32(2))})
	if err != nil || len(p.Messages) != 2 {
		t.Fatalf("peek: %v %d", err, len(p.Messages))
	}
	if *p.Messages[0].MessageText != "m0" || *p.Messages[0].DequeueCount != 0 {
		t.Fatalf("peeked %+v", p.Messages[0])
	}
	d, _ := q.DequeueMessages(ctx, &azqueue.DequeueMessagesOptions{NumberOfMessages: ptr(int32(3))})
	if len(d.Messages) != 3 {
		t.Fatalf("dequeued %d", len(d.Messages))
	}
}

func TestQueue_UpdateMessageChangesTextAndGivesANewPopReceipt(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "upd")
	q.EnqueueMessage(ctx, "before", nil)
	d, _ := q.DequeueMessage(ctx, &azqueue.DequeueMessageOptions{VisibilityTimeout: ptr(int32(30))})
	m := d.Messages[0]
	up, err := q.UpdateMessage(ctx, *m.MessageID, *m.PopReceipt, "after", &azqueue.UpdateMessageOptions{VisibilityTimeout: ptr(int32(0))})
	if err != nil {
		t.Fatal(err)
	}
	if up.PopReceipt == nil || *up.PopReceipt == *m.PopReceipt {
		t.Fatal("pop receipt should change")
	}
	d2, _ := q.DequeueMessage(ctx, nil)
	if len(d2.Messages) != 1 || *d2.Messages[0].MessageText != "after" {
		t.Fatalf("after update: %+v", d2.Messages)
	}
}

func TestQueue_MessageWithVisibilityTimeoutOnEnqueueIsHiddenAtFirst(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "delay")
	q.EnqueueMessage(ctx, "later", &azqueue.EnqueueMessageOptions{VisibilityTimeout: ptr(int32(2))})
	d, _ := q.DequeueMessage(ctx, nil)
	if len(d.Messages) != 0 {
		t.Fatal("delayed message visible at once")
	}
	time.Sleep(2200 * time.Millisecond)
	d, _ = q.DequeueMessage(ctx, nil)
	if len(d.Messages) != 1 {
		t.Fatal("message never became visible")
	}
}

func TestQueue_TimeToLiveOfOneSecondExpiresTheMessage(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "ttl")
	q.EnqueueMessage(ctx, "short lived", &azqueue.EnqueueMessageOptions{TimeToLive: ptr(int32(1))})
	time.Sleep(2200 * time.Millisecond)
	d, _ := q.DequeueMessage(ctx, nil)
	if len(d.Messages) != 0 {
		t.Fatal("expired message delivered")
	}
}

func TestQueue_InfiniteTimeToLiveIsAccepted(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "ttlinf")
	if _, err := q.EnqueueMessage(ctx, "forever", &azqueue.EnqueueMessageOptions{TimeToLive: ptr(int32(-1))}); err != nil {
		t.Fatal(err)
	}
	p, _ := q.PeekMessage(ctx, nil)
	if len(p.Messages) != 1 {
		t.Fatal("no message")
	}
}

func TestQueue_PropertiesReportTheApproximateMessageCountAndMetadata(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "props")
	for i := 0; i < 4; i++ {
		q.EnqueueMessage(ctx, "m", nil)
	}
	if _, err := q.SetMetadata(ctx, &azqueue.SetMetadataOptions{Metadata: map[string]*string{"team": ptr("blue")}}); err != nil {
		t.Fatal(err)
	}
	p, err := q.GetProperties(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if *p.ApproximateMessagesCount != 4 {
		t.Fatalf("count %d", *p.ApproximateMessagesCount)
	}
	got := ""
	for k, v := range p.Metadata {
		if strings.EqualFold(k, "team") {
			got = *v
		}
	}
	if got != "blue" {
		t.Fatalf("metadata %v", p.Metadata)
	}
}

func TestQueue_ClearMessagesEmptiesTheQueue(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "clear")
	for i := 0; i < 3; i++ {
		q.EnqueueMessage(ctx, "m", nil)
	}
	if _, err := q.ClearMessages(ctx, nil); err != nil {
		t.Fatal(err)
	}
	p, _ := q.GetProperties(ctx, nil)
	if *p.ApproximateMessagesCount != 0 {
		t.Fatalf("count %d", *p.ApproximateMessagesCount)
	}
}

func TestQueue_MessagesAreDeliveredInFirstInFirstOutOrder(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "fifo")
	for i := 0; i < 10; i++ {
		q.EnqueueMessage(ctx, fmt.Sprintf("m%02d", i), nil)
	}
	d, _ := q.DequeueMessages(ctx, &azqueue.DequeueMessagesOptions{NumberOfMessages: ptr(int32(10))})
	for i, m := range d.Messages {
		if *m.MessageText != fmt.Sprintf("m%02d", i) {
			t.Fatalf("position %d got %s", i, *m.MessageText)
		}
	}
}

func TestQueue_XmlSpecialCharactersAndUnicodeInTheMessageRoundTrip(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "xml")
	text := `<a href="x">&amp; 'quotes' ünï 日本語 😀 </a>`
	q.EnqueueMessage(ctx, text, nil)
	d, _ := q.DequeueMessage(ctx, nil)
	if len(d.Messages) != 1 || *d.Messages[0].MessageText != text {
		t.Fatalf("got %+v", d.Messages)
	}
}

func TestQueue_MessageOverSixtyFourKiBIsRequestBodyTooLarge413(t *testing.T) {
	q := newQueue(t, "big")
	_, err := q.EnqueueMessage(context.Background(), strings.Repeat("x", 70*1024), nil)
	wantCode(t, err, http.StatusRequestEntityTooLarge, queueerror.Code("RequestBodyTooLarge"))
}

func TestQueue_MissingQueueIsQueueNotFound404(t *testing.T) {
	q := svc(t).NewQueueClient(emu.Name("ghost"))
	_, err := q.EnqueueMessage(context.Background(), "m", nil)
	wantCode(t, err, http.StatusNotFound, queueerror.QueueNotFound)
}

func TestQueue_DeleteQueueThenOperationsFail(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "delq")
	if _, err := q.Delete(ctx, nil); err != nil {
		t.Fatal(err)
	}
	_, err := q.GetProperties(ctx, nil)
	wantCode(t, err, http.StatusNotFound, queueerror.QueueNotFound)
}

func TestQueue_ListQueuesWithPrefixAndPaging(t *testing.T) {
	ctx := context.Background()
	s := svc(t)
	pre := emu.Name("lq")
	for i := 0; i < 5; i++ {
		s.CreateQueue(ctx, fmt.Sprintf("%s-%d", pre, i), nil)
	}
	pages, n := 0, 0
	for p := s.NewListQueuesPager(&azqueue.ListQueuesOptions{Prefix: &pre, MaxResults: ptr(int32(2))}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		n += len(pg.Queues)
	}
	if n != 5 || pages != 3 {
		t.Fatalf("n=%d pages=%d", n, pages)
	}
}

func TestSAS_GetSASURLSignsForHttpsOnlySoAPlainHttpRequestIsAuthorizationProtocolMismatch403(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "sashttps")
	u, err := q.GetSASURL(sas.QueuePermissions{Read: true, Process: true}, time.Now().Add(time.Hour), nil)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(u, "spr=https") {
		t.Skip("this SDK version signs for HTTP too")
	}
	qs, _ := azqueue.NewQueueClientWithNoCredential(u, nil)
	_, err = qs.DequeueMessage(ctx, nil)
	wantCode(t, err, http.StatusForbidden, queueerror.Code("AuthorizationProtocolMismatch"))
}

func TestSAS_QueueSasWithProcessPermissionDequeuesButCannotAdd(t *testing.T) {
	ctx := context.Background()
	q := newQueue(t, "sas")
	q.EnqueueMessage(ctx, "for sas", nil)
	name := strings.TrimPrefix(q.URL(), emu.E.HTTP("queue", emu.DevAccount)+"/")
	qp, err := sas.QueueSignatureValues{Protocol: sas.ProtocolHTTPSandHTTP, ExpiryTime: time.Now().UTC().Add(time.Hour), // azqueue/sas writes the wall clock of the time it is given as UTC, so pass UTC
		Permissions: (&sas.QueuePermissions{Read: true, Process: true}).String(), QueueName: name}.SignWithSharedKey(cred(t))
	if err != nil {
		t.Fatal(err)
	}
	qs, err := azqueue.NewQueueClientWithNoCredential(q.URL()+"?"+qp.Encode(), nil)
	if err != nil {
		t.Fatal(err)
	}
	d, err := qs.DequeueMessage(ctx, nil)
	if err != nil || len(d.Messages) != 1 {
		t.Fatalf("dequeue with SAS: %v", err)
	}
	_, err = qs.EnqueueMessage(ctx, "nope", nil)
	wantCode(t, err, http.StatusForbidden, queueerror.Code("AuthorizationPermissionMismatch"))
}

func TestSharedKey_WrongKeyIsAuthenticationFailed403(t *testing.T) {
	bad, err := azqueue.NewSharedKeyCredential(emu.DevAccount, "eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eA==")
	if err != nil {
		t.Fatal(err)
	}
	s, _ := azqueue.NewServiceClientWithSharedKeyCredential(emu.E.HTTP("queue", emu.DevAccount), bad, nil)
	_, err = s.CreateQueue(context.Background(), emu.Name("badkey"), nil)
	wantCode(t, err, http.StatusForbidden, queueerror.AuthenticationFailed)
}
