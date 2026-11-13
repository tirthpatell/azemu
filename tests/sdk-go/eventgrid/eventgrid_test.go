package eventgrid

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/messaging"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/eventgrid/azeventgrid"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/eventgrid/aznamespaces"
)

func TestMain(m *testing.M) { emu.Main(m) }

func ptr[T any](v T) *T { return &v }

var key = azcore.NewKeyCredential("any-key")

func httpOpts() azcore.ClientOptions {
	return azcore.ClientOptions{Transport: &http.Client{Transport: emu.Dump(emu.E.NewTransport())}}
}

// admin sends a JSON request to the emulator's admin API (plain HTTP on the gateway); the Event Grid SDKs cannot create
// topics and subscriptions, ARM does on Azure.
func admin(t testing.TB, method, path string, body any) map[string]any {
	t.Helper()
	var rdr io.Reader
	if body != nil {
		b, _ := json.Marshal(body)
		rdr = bytes.NewReader(b)
	}
	req, _ := http.NewRequest(method, emu.E.Gateway()+"/_azemu/eventgrid"+path, rdr)
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	if resp.StatusCode >= 300 {
		t.Fatalf("%s %s: %d %s", method, path, resp.StatusCode, b)
	}
	var m map[string]any
	json.Unmarshal(b, &m)
	return m
}

// hook is a webhook that records what the emulator delivers.
type hook struct {
	srv *httptest.Server
	mu  sync.Mutex
	got [][]byte
	hdr []http.Header
}

func newHook(t testing.TB) *hook {
	h := &hook{}
	h.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		h.mu.Lock()
		h.got = append(h.got, b)
		h.hdr = append(h.hdr, r.Header.Clone())
		h.mu.Unlock()
		w.WriteHeader(200)
	}))
	t.Cleanup(h.srv.Close)
	return h
}

func (h *hook) wait(t testing.TB, n int) [][]byte {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	for {
		h.mu.Lock()
		c := len(h.got)
		out := append([][]byte(nil), h.got...)
		h.mu.Unlock()
		if c >= n {
			return out
		}
		if time.Now().After(deadline) {
			t.Fatalf("webhook got %d deliveries, want %d", c, n)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestPublish_EventGridSchemaEventsReachAWebhookSubscriptionOfTheTopic(t *testing.T) {
	topic := emu.Name("egt")
	admin(t, "PUT", "/topics/"+topic, map[string]any{"kind": "custom"})
	h := newHook(t)
	admin(t, "PUT", "/topics/"+topic+"/subscriptions/hook", map[string]any{"webhookUrl": h.srv.URL, "skipValidation": true, "deliverySchema": "EventGridSchema"})
	c, err := azeventgrid.NewClientWithSharedKeyCredential(emu.E.HTTPS("eventgrid", topic)+"/api/events", key, &azeventgrid.ClientOptions{ClientOptions: httpOpts()})
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now().UTC()
	_, err = c.PublishEvents(context.Background(), []azeventgrid.Event{{
		ID: ptr("evt-1"), Subject: ptr("/orders/42"), EventType: ptr("Order.Created"), EventTime: &now, DataVersion: ptr("1.0"), Data: map[string]any{"qty": 3, "sku": "A-1"},
	}}, nil)
	if err != nil {
		t.Fatalf("publish: %v", err)
	}
	got := h.wait(t, 1)
	var evs []map[string]any
	if err := json.Unmarshal(got[0], &evs); err != nil || len(evs) != 1 {
		t.Fatalf("delivery body %s: %v", got[0], err)
	}
	e := evs[0]
	if e["id"] != "evt-1" || e["subject"] != "/orders/42" || e["eventType"] != "Order.Created" || e["dataVersion"] != "1.0" {
		t.Fatalf("event %v", e)
	}
	if e["data"].(map[string]any)["sku"] != "A-1" {
		t.Fatalf("data %v", e["data"])
	}
	if h.hdr[0].Get("aeg-event-type") != "Notification" {
		t.Fatalf("headers %v", h.hdr[0])
	}
}

func TestPublish_CloudEventsBatchIsDeliveredAsCloudEventsToAWebhook(t *testing.T) {
	topic := emu.Name("egc")
	admin(t, "PUT", "/topics/"+topic, map[string]any{"kind": "custom", "inputSchema": "CloudEventSchemaV1_0"})
	h := newHook(t)
	admin(t, "PUT", "/topics/"+topic+"/subscriptions/hook", map[string]any{"webhookUrl": h.srv.URL, "skipValidation": true, "deliverySchema": "CloudEventSchemaV1_0"})
	c, _ := azeventgrid.NewClientWithSharedKeyCredential(emu.E.HTTPS("eventgrid", topic)+"/api/events", key, &azeventgrid.ClientOptions{ClientOptions: httpOpts()})
	var evs []messaging.CloudEvent
	for i := 0; i < 3; i++ {
		ce, err := messaging.NewCloudEvent("/sources/test", "My.Type", map[string]any{"n": i}, &messaging.CloudEventOptions{Subject: ptr(fmt.Sprintf("s%d", i))})
		if err != nil {
			t.Fatal(err)
		}
		evs = append(evs, ce)
	}
	if _, err := c.PublishCloudEvents(context.Background(), evs, nil); err != nil {
		t.Fatalf("publish: %v", err)
	}
	got := h.wait(t, 3)
	for _, g := range got {
		var m map[string]any
		if err := json.Unmarshal(g, &m); err != nil {
			t.Fatalf("%s", g)
		}
		if m["specversion"] != "1.0" || m["type"] != "My.Type" || m["source"] != "/sources/test" {
			t.Fatalf("cloud event %v", m)
		}
	}
}

func TestPublish_EventMissingARequiredFieldIsBadRequest400(t *testing.T) {
	topic := emu.Name("egbad")
	admin(t, "PUT", "/topics/"+topic, map[string]any{"kind": "custom", "inputSchema": "EventGridSchema"})
	c, _ := azeventgrid.NewClientWithSharedKeyCredential(emu.E.HTTPS("eventgrid", topic)+"/api/events", key, &azeventgrid.ClientOptions{ClientOptions: httpOpts()})
	// no subject, no event type
	_, err := c.PublishCustomEventEvents(context.Background(), []any{map[string]any{"id": "1", "dataVersion": "1", "eventTime": time.Now().UTC().Format(time.RFC3339)}}, nil)
	var re *azcore.ResponseError
	if !asRE(err, &re) || re.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400, got %v", err)
	}
}

func TestPublish_ToATopicWithoutACredentialHeaderIsUnauthorized401(t *testing.T) {
	topic := emu.Name("egauth")
	req, _ := http.NewRequest("POST", emu.E.HTTP("eventgrid", topic)+"/api/events", bytes.NewReader([]byte(`[]`)))
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("status %d", resp.StatusCode)
	}
}

func TestPublish_WithABearerTokenCredentialOverHttps(t *testing.T) {
	topic := emu.Name("egtok")
	admin(t, "PUT", "/topics/"+topic, map[string]any{"kind": "custom"})
	c, err := azeventgrid.NewClient(emu.E.HTTPS("eventgrid", topic)+"/api/events", emu.Cred(), &azeventgrid.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: emu.E.Transport()}})
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now().UTC()
	if _, err := c.PublishEvents(context.Background(), []azeventgrid.Event{{ID: ptr("1"), Subject: ptr("s"), EventType: ptr("t"), EventTime: &now, DataVersion: ptr("1"), Data: "x"}}, nil); err != nil {
		t.Fatalf("publish with a token: %v", err)
	}
}

// ---- namespace topics: pull delivery ----

func nsClients(t testing.TB, ns, topic, sub string) (*aznamespaces.SenderClient, *aznamespaces.ReceiverClient) {
	t.Helper()
	opts := azcore.ClientOptions{Transport: &http.Client{Transport: emu.Dump(emu.E.NewTransport())}}
	endpoint := emu.E.HTTPS("eventgrid", ns)
	s, err := aznamespaces.NewSenderClientWithSharedKeyCredential(endpoint, topic, key, &aznamespaces.SenderClientOptions{ClientOptions: opts})
	if err != nil {
		t.Fatal(err)
	}
	r, err := aznamespaces.NewReceiverClientWithSharedKeyCredential(endpoint, topic, sub, key, &aznamespaces.ReceiverClientOptions{ClientOptions: opts})
	if err != nil {
		t.Fatal(err)
	}
	return s, r
}

func newPullTopic(t testing.TB, ns, topic, sub string, sub_cfg map[string]any) {
	admin(t, "PUT", "/topics/"+topic+"?namespace="+ns, map[string]any{"kind": "namespace"})
	cfg := map[string]any{"deliveryMode": "pull"}
	for k, v := range sub_cfg {
		cfg[k] = v
	}
	admin(t, "PUT", "/topics/"+topic+"/subscriptions/"+sub+"?namespace="+ns, cfg)
}

func cloud(t testing.TB, typ string, data any) *messaging.CloudEvent {
	ce, err := messaging.NewCloudEvent("/test", typ, data, nil)
	if err != nil {
		t.Fatal(err)
	}
	return &ce
}

func TestPull_SendReceiveAcknowledgeLeavesNothingBehind(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", nil)
	s, r := nsClients(t, ns, "jobs", "workers")
	if _, err := s.SendEvent(ctx, cloud(t, "Job.Created", map[string]any{"id": 7}), nil); err != nil {
		t.Fatalf("send: %v", err)
	}
	res, err := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxEvents: ptr(int32(5)), MaxWaitTime: ptr(int32(10))})
	if err != nil || len(res.Details) != 1 {
		t.Fatalf("receive: %v %+v", err, res)
	}
	d := res.Details[0]
	if d.Event.Type != "Job.Created" || d.BrokerProperties == nil || *d.BrokerProperties.DeliveryCount != 1 || *d.BrokerProperties.LockToken == "" {
		t.Fatalf("details %+v", d)
	}
	var data map[string]any
	if err := json.Unmarshal(d.Event.Data.([]byte), &data); err != nil {
		// data may arrive decoded already
		if m, ok := d.Event.Data.(map[string]any); ok {
			data = m
		} else {
			t.Fatalf("event data %T", d.Event.Data)
		}
	}
	if data["id"] != float64(7) {
		t.Fatalf("data %v", data)
	}
	ack, err := r.AcknowledgeEvents(ctx, []string{*d.BrokerProperties.LockToken}, nil)
	if err != nil || len(ack.SucceededLockTokens) != 1 || len(ack.FailedLockTokens) != 0 {
		t.Fatalf("ack: %v %+v", err, ack)
	}
	again, err := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(1))})
	if err != nil || len(again.Details) != 0 {
		t.Fatalf("after ack: %v %d", err, len(again.Details))
	}
}

func TestPull_ReleaseMakesTheEventAvailableAgainWithADeliveryCountOfTwo(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", nil)
	s, r := nsClients(t, ns, "jobs", "workers")
	s.SendEvent(ctx, cloud(t, "Job.Created", "x"), nil)
	res, _ := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(10))})
	if len(res.Details) != 1 {
		t.Fatal("no event")
	}
	rel, err := r.ReleaseEvents(ctx, []string{*res.Details[0].BrokerProperties.LockToken}, nil)
	if err != nil || len(rel.SucceededLockTokens) != 1 {
		t.Fatalf("release: %v %+v", err, rel)
	}
	res2, err := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(10))})
	if err != nil || len(res2.Details) != 1 || *res2.Details[0].BrokerProperties.DeliveryCount != 2 {
		t.Fatalf("second receive: %v %+v", err, res2.Details)
	}
}

func TestPull_RejectDeadLettersTheEvent(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", nil)
	s, r := nsClients(t, ns, "jobs", "workers")
	s.SendEvent(ctx, cloud(t, "Job.Bad", "x"), nil)
	res, _ := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(10))})
	rej, err := r.RejectEvents(ctx, []string{*res.Details[0].BrokerProperties.LockToken}, nil)
	if err != nil || len(rej.SucceededLockTokens) != 1 {
		t.Fatalf("reject: %v %+v", err, rej)
	}
	again, _ := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(1))})
	if len(again.Details) != 0 {
		t.Fatal("rejected event came back")
	}
}

func TestPull_AcknowledgeWithAnUnknownTokenIsReportedAsFailedLockTokenNotAnError(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", nil)
	_, r := nsClients(t, ns, "jobs", "workers")
	ack, err := r.AcknowledgeEvents(ctx, []string{"bm90LWEtdG9rZW4="}, nil)
	if err != nil {
		t.Fatalf("a bad token is a per-token failure, got error %v", err)
	}
	if len(ack.FailedLockTokens) != 1 || len(ack.SucceededLockTokens) != 0 {
		t.Fatalf("%+v", ack)
	}
}

func TestPull_ExpiredLockOfAShortLivedSubscriptionOffersTheEventAgain(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", map[string]any{"lockDurationSeconds": 1})
	s, r := nsClients(t, ns, "jobs", "workers")
	s.SendEvent(ctx, cloud(t, "Job.Slow", "x"), nil)
	first, _ := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(10))})
	if len(first.Details) != 1 {
		t.Fatal("no event")
	}
	time.Sleep(1500 * time.Millisecond)
	second, err := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(10))})
	if err != nil || len(second.Details) != 1 || *second.Details[0].BrokerProperties.DeliveryCount != 2 {
		t.Fatalf("after lock expiry: %v %+v", err, second.Details)
	}
	// the old token is no longer valid
	ack, _ := r.AcknowledgeEvents(ctx, []string{*first.Details[0].BrokerProperties.LockToken}, nil)
	if len(ack.FailedLockTokens) != 1 {
		t.Fatalf("old token acknowledged: %+v", ack)
	}
}

func TestPull_RenewLockExtendsTheLockPastItsOriginalExpiry(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", map[string]any{"lockDurationSeconds": 2})
	s, r := nsClients(t, ns, "jobs", "workers")
	s.SendEvent(ctx, cloud(t, "Job.Long", "x"), nil)
	res, _ := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxWaitTime: ptr(int32(10))})
	tok := *res.Details[0].BrokerProperties.LockToken
	time.Sleep(1200 * time.Millisecond)
	renew, err := r.RenewEventLocks(ctx, []string{tok}, nil)
	if err != nil || len(renew.SucceededLockTokens) != 1 {
		t.Fatalf("renew: %v %+v", err, renew)
	}
	time.Sleep(1200 * time.Millisecond) // past the first expiry, inside the renewed one
	ack, err := r.AcknowledgeEvents(ctx, []string{tok}, nil)
	if err != nil || len(ack.SucceededLockTokens) != 1 {
		t.Fatalf("ack after renew: %v %+v", err, ack)
	}
}

func TestPull_ManyEventsInOneSendAreReceivedInOrderInBatches(t *testing.T) {
	ctx := context.Background()
	ns := emu.Name("egns")
	newPullTopic(t, ns, "jobs", "workers", nil)
	s, r := nsClients(t, ns, "jobs", "workers")
	var evs []*messaging.CloudEvent
	for i := 0; i < 25; i++ {
		ce := cloud(t, fmt.Sprintf("T.%02d", i), i)
		evs = append(evs, ce)
	}
	if _, err := s.SendEvents(ctx, evs, nil); err != nil {
		t.Fatal(err)
	}
	var types []string
	for len(types) < 25 {
		res, err := r.ReceiveEvents(ctx, &aznamespaces.ReceiveEventsOptions{MaxEvents: ptr(int32(10)), MaxWaitTime: ptr(int32(5))})
		if err != nil || len(res.Details) == 0 {
			t.Fatalf("receive after %d: %v", len(types), err)
		}
		var tokens []string
		for _, d := range res.Details {
			types = append(types, d.Event.Type)
			tokens = append(tokens, *d.BrokerProperties.LockToken)
		}
		r.AcknowledgeEvents(ctx, tokens, nil)
	}
	for i, ty := range types {
		if ty != fmt.Sprintf("T.%02d", i) {
			t.Fatalf("order at %d: %s", i, ty)
		}
	}
}

func asRE(err error, target **azcore.ResponseError) bool {
	if err == nil {
		return false
	}
	return errorsAs(err, target)
}
