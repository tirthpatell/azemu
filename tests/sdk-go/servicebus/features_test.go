package servicebus

import (
	"context"
	"fmt"
	"sort"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus/admin"
)

func TestQueue_AbandonUntilMaxDeliveryCountMovesTheMessageToTheDeadLetterQueue(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	newAdmin(t, ns).CreateQueue(ctx, "maxdel", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{MaxDeliveryCount: ptr(int32(3))}})
	c := newClient(t, ns)
	send(t, c, "maxdel", text("loop"))
	r := newReceiver(t, c, "maxdel", nil)
	for i := 1; i <= 3; i++ {
		m := recvOne(t, r)
		if m.DeliveryCount != uint32(i) {
			t.Fatalf("delivery %d has count %d", i, m.DeliveryCount)
		}
		r.AbandonMessage(ctx, m, nil)
	}
	none, _ := r.ReceiveMessages(ctxT(t, 2*time.Second), 1, nil)
	if len(none) != 0 {
		t.Fatal("message delivered a fourth time")
	}
	dlq := newReceiver(t, c, "maxdel", &azservicebus.ReceiverOptions{SubQueue: azservicebus.SubQueueDeadLetter})
	dm := recvOne(t, dlq)
	if *dm.DeadLetterReason != "MaxDeliveryCountExceeded" {
		t.Fatalf("reason %v", dm.DeadLetterReason)
	}
}

func TestQueue_ExpiredLockReturnsTheMessageAndCompletingTheOldLockFailsWithLockLost(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	newAdmin(t, ns).CreateQueue(ctx, "lockexp", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{LockDuration: ptr("PT5S")}})
	c := newClient(t, ns)
	send(t, c, "lockexp", text("slow consumer"))
	r := newReceiver(t, c, "lockexp", nil)
	m := recvOne(t, r)
	time.Sleep(6 * time.Second)
	err := r.CompleteMessage(ctx, m, nil)
	e := asSBError(err)
	if e == nil || e.Code != azservicebus.CodeLockLost {
		t.Fatalf("complete after lock expiry: %v", err)
	}
	m2 := recvOne(t, r)
	if string(m2.Body) != "slow consumer" || m2.DeliveryCount < 2 {
		t.Fatalf("redelivery %q count %d", m2.Body, m2.DeliveryCount)
	}
	r.CompleteMessage(ctx, m2, nil)
}

func TestQueue_RenewMessageLockExtendsTheLock(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	newAdmin(t, ns).CreateQueue(ctx, "renew", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{LockDuration: ptr("PT5S")}})
	c := newClient(t, ns)
	send(t, c, "renew", text("m"))
	r := newReceiver(t, c, "renew", nil)
	m := recvOne(t, r)
	first := *m.LockedUntil
	time.Sleep(3 * time.Second)
	if err := r.RenewMessageLock(ctx, m, nil); err != nil {
		t.Fatalf("renew: %v", err)
	}
	if !m.LockedUntil.After(first) {
		t.Fatalf("lock not extended: %v vs %v", m.LockedUntil, first)
	}
	time.Sleep(3 * time.Second) // past the original expiry
	if err := r.CompleteMessage(ctx, m, nil); err != nil {
		t.Fatalf("complete after renew: %v", err)
	}
}

func TestQueue_DuplicateDetectionDropsASecondMessageWithTheSameMessageId(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	newAdmin(t, ns).CreateQueue(ctx, "dupdet", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{RequiresDuplicateDetection: ptr(true), DuplicateDetectionHistoryTimeWindow: ptr("PT5M")}})
	c := newClient(t, ns)
	send(t, c, "dupdet", &azservicebus.Message{Body: []byte("one"), MessageID: ptr("same")}, &azservicebus.Message{Body: []byte("two"), MessageID: ptr("same")}, &azservicebus.Message{Body: []byte("three"), MessageID: ptr("other")})
	r := newReceiver(t, c, "dupdet", &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
	ms, _ := r.ReceiveMessages(ctxT(t, 3*time.Second), 5, nil)
	var bodies []string
	for _, m := range ms {
		bodies = append(bodies, string(m.Body))
	}
	sort.Strings(bodies)
	if fmt.Sprint(bodies) != "[one three]" {
		t.Fatalf("got %v", bodies)
	}
}

func TestQueue_DeadLetteringOnExpirationMovesAnExpiredMessageToTheDeadLetterQueue(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	newAdmin(t, ns).CreateQueue(ctx, "dlexp", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{DeadLetteringOnMessageExpiration: ptr(true)}})
	c := newClient(t, ns)
	send(t, c, "dlexp", &azservicebus.Message{Body: []byte("short"), TimeToLive: ptr(time.Second)})
	time.Sleep(2500 * time.Millisecond)
	dlq := newReceiver(t, c, "dlexp", &azservicebus.ReceiverOptions{SubQueue: azservicebus.SubQueueDeadLetter})
	dm := recvOne(t, dlq)
	if dm.DeadLetterReason == nil || *dm.DeadLetterReason != "TTLExpiredException" {
		t.Fatalf("reason %v", dm.DeadLetterReason)
	}
}

func TestQueue_AutoForwardMovesMessagesToTheTargetQueue(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	a := newAdmin(t, ns)
	a.CreateQueue(ctx, "fwd-target", nil)
	if _, err := a.CreateQueue(ctx, "fwd-source", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{ForwardTo: ptr("fwd-target")}}); err != nil {
		t.Fatal(err)
	}
	c := newClient(t, ns)
	send(t, c, "fwd-source", text("hop"))
	r := newReceiver(t, c, "fwd-target", nil)
	m := recvOne(t, r)
	if string(m.Body) != "hop" {
		t.Fatalf("%q", m.Body)
	}
	r.CompleteMessage(ctx, m, nil)
}

// ---- sessions ----

func newSessionQueue(t testing.TB, ns, name string) {
	t.Helper()
	if _, err := newAdmin(t, ns).CreateQueue(context.Background(), name, &admin.CreateQueueOptions{Properties: &admin.QueueProperties{RequiresSession: ptr(true)}}); err != nil {
		t.Fatal(err)
	}
}

func sessMsg(session, body string) *azservicebus.Message {
	return &azservicebus.Message{Body: []byte(body), SessionID: ptr(session)}
}

func TestSession_AcceptSessionReceivesOnlyThatSessionsMessagesInOrder(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq1")
	c := newClient(t, ns)
	send(t, c, "sq1", sessMsg("A", "a1"), sessMsg("B", "b1"), sessMsg("A", "a2"), sessMsg("B", "b2"), sessMsg("A", "a3"))
	sr, err := c.AcceptSessionForQueue(ctxT(t, 20*time.Second), "sq1", "A", nil)
	if err != nil {
		t.Fatalf("accept session: %v", err)
	}
	defer sr.Close(context.Background())
	if sr.SessionID() != "A" {
		t.Fatalf("session id %q", sr.SessionID())
	}
	ms, err := sr.ReceiveMessages(ctxT(t, 10*time.Second), 3, nil)
	if err != nil || len(ms) != 3 {
		t.Fatalf("receive: %v %d", err, len(ms))
	}
	for i, want := range []string{"a1", "a2", "a3"} {
		if string(ms[i].Body) != want || ms[i].SessionID == nil || *ms[i].SessionID != "A" {
			t.Fatalf("message %d: %q session %v", i, ms[i].Body, ms[i].SessionID)
		}
		sr.CompleteMessage(context.Background(), ms[i], nil)
	}
}

func TestSession_AcceptNextSessionPicksAnAvailableSessionAndTwoReceiversGetDifferentOnes(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq2")
	c := newClient(t, ns)
	send(t, c, "sq2", sessMsg("one", "1"), sessMsg("two", "2"))
	r1, err := c.AcceptNextSessionForQueue(ctxT(t, 20*time.Second), "sq2", nil)
	if err != nil {
		t.Fatalf("first: %v", err)
	}
	defer r1.Close(context.Background())
	r2, err := c.AcceptNextSessionForQueue(ctxT(t, 20*time.Second), "sq2", nil)
	if err != nil {
		t.Fatalf("second: %v", err)
	}
	defer r2.Close(context.Background())
	if r1.SessionID() == r2.SessionID() {
		t.Fatalf("both receivers got session %q", r1.SessionID())
	}
}

func TestSession_AcceptNextSessionWhenNoneHaveMessagesTimesOutWithNoSessionAvailable(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq3")
	c := newClient(t, ns)
	start := time.Now()
	// The Go SDK copies the deadline of its context into the wait it asks the service for, so the context is
	// the wait: a short one, and the bound below is above it.
	_, err := c.AcceptNextSessionForQueue(ctxT(t, 8*time.Second), "sq3", nil)
	if err == nil {
		t.Fatal("expected an error")
	}
	e := asSBError(err)
	if e == nil || e.Code != azservicebus.CodeTimeout && e.Code != "" {
		t.Logf("error %v after %v", err, time.Since(start))
	}
	if time.Since(start) > 15*time.Second {
		t.Fatalf("took %v", time.Since(start))
	}
}

func TestSession_StateIsStoredPerSessionAndSurvivesReceivers(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq4")
	c := newClient(t, ns)
	send(t, c, "sq4", sessMsg("S", "x"))
	sr, err := c.AcceptSessionForQueue(ctxT(t, 20*time.Second), "sq4", "S", nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := sr.SetSessionState(ctxT(t, 10*time.Second), []byte("checkpoint-7"), nil); err != nil {
		t.Fatalf("set state: %v", err)
	}
	sr.Close(context.Background())
	sr2, err := c.AcceptSessionForQueue(ctxT(t, 20*time.Second), "sq4", "S", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer sr2.Close(context.Background())
	got, err := sr2.GetSessionState(ctxT(t, 10*time.Second), nil)
	if err != nil || string(got) != "checkpoint-7" {
		t.Fatalf("state %q %v", got, err)
	}
	// clearing
	sr2.SetSessionState(ctxT(t, 10*time.Second), nil, nil)
	got, _ = sr2.GetSessionState(ctxT(t, 10*time.Second), nil)
	if len(got) != 0 {
		t.Fatalf("state after clear %q", got)
	}
}

func TestSession_SecondReceiverForAHeldSessionIsRefused(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq5")
	c := newClient(t, ns)
	send(t, c, "sq5", sessMsg("held", "x"))
	sr, err := c.AcceptSessionForQueue(ctxT(t, 20*time.Second), "sq5", "held", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer sr.Close(context.Background())
	_, err = c.AcceptSessionForQueue(ctxT(t, 20*time.Second), "sq5", "held", nil)
	if err == nil {
		t.Fatal("a held session must not be accepted twice")
	}
	if e := asSBError(err); e == nil || e.Code != azservicebus.CodeLockLost && e.Code != azservicebus.CodeTimeout {
		t.Logf("error: %v", err)
	}
}

func TestSession_RenewSessionLockMovesLockedUntilForward(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq6")
	c := newClient(t, ns)
	send(t, c, "sq6", sessMsg("R", "x"))
	sr, err := c.AcceptSessionForQueue(ctxT(t, 20*time.Second), "sq6", "R", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer sr.Close(context.Background())
	before := sr.LockedUntil()
	time.Sleep(1100 * time.Millisecond)
	if err := sr.RenewSessionLock(ctxT(t, 10*time.Second), nil); err != nil {
		t.Fatalf("renew: %v", err)
	}
	if !sr.LockedUntil().After(before) {
		t.Fatalf("locked until %v not after %v", sr.LockedUntil(), before)
	}
}

func TestSession_ListSessionsShowsSessionsWithMessages(t *testing.T) {
	ns := newNS()
	newSessionQueue(t, ns, "sq7")
	c := newClient(t, ns)
	send(t, c, "sq7", sessMsg("x1", "a"), sessMsg("x2", "b"), sessMsg("x3", "c"))
	var ids []string
	for p := c.NewListSessionsForQueuePager("sq7", nil); p.More(); {
		pg, err := p.NextPage(ctxT(t, 10*time.Second))
		if err != nil {
			t.Fatalf("list sessions: %v", err)
		}
		ids = append(ids, pg.Sessions...)
	}
	sort.Strings(ids)
	if fmt.Sprint(ids) != "[x1 x2 x3]" {
		t.Fatalf("sessions %v", ids)
	}
}

// ---- topics ----

func TestTopic_EachSubscriptionGetsItsOwnCopyOfEveryMessage(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	a := newAdmin(t, ns)
	a.CreateTopic(ctx, "fanout", nil)
	a.CreateSubscription(ctx, "fanout", "s1", nil)
	a.CreateSubscription(ctx, "fanout", "s2", nil)
	c := newClient(t, ns)
	send(t, c, "fanout", text("broadcast"))
	for _, sub := range []string{"s1", "s2"} {
		r, err := c.NewReceiverForSubscription("fanout", sub, nil)
		if err != nil {
			t.Fatal(err)
		}
		m := recvOne(t, r)
		if string(m.Body) != "broadcast" {
			t.Fatalf("%s: %q", sub, m.Body)
		}
		r.CompleteMessage(ctx, m, nil)
		r.Close(ctx)
	}
}

func TestTopic_SqlAndCorrelationRulesRouteMessagesAndTheActionAddsAProperty(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	a := newAdmin(t, ns)
	a.CreateTopic(ctx, "routed", nil)
	for _, s := range []string{"red", "corr", "all"} {
		a.CreateSubscription(ctx, "routed", s, nil)
	}
	a.DeleteRule(ctx, "routed", "red", "$Default", nil)
	a.DeleteRule(ctx, "routed", "corr", "$Default", nil)
	a.CreateRule(ctx, "routed", "red", &admin.CreateRuleOptions{Name: ptr("r"), Filter: &admin.SQLFilter{Expression: "color = 'red' AND qty >= @min", Parameters: map[string]any{"@min": int64(3)}}, Action: &admin.SQLAction{Expression: "SET routed = 'yes'"}})
	a.CreateRule(ctx, "routed", "corr", &admin.CreateRuleOptions{Name: ptr("c"), Filter: &admin.CorrelationFilter{Subject: ptr("sale"), ApplicationProperties: map[string]any{"region": "eu"}}})
	c := newClient(t, ns)
	send(t, c, "routed",
		&azservicebus.Message{Body: []byte("m1"), ApplicationProperties: map[string]any{"color": "red", "qty": int64(5)}},
		&azservicebus.Message{Body: []byte("m2"), ApplicationProperties: map[string]any{"color": "red", "qty": int64(1)}},
		&azservicebus.Message{Body: []byte("m3"), ApplicationProperties: map[string]any{"color": "blue", "qty": int64(9)}},
		&azservicebus.Message{Body: []byte("m4"), Subject: ptr("sale"), ApplicationProperties: map[string]any{"region": "eu"}},
		&azservicebus.Message{Body: []byte("m5"), Subject: ptr("sale"), ApplicationProperties: map[string]any{"region": "us"}},
	)
	got := func(sub string) []*azservicebus.ReceivedMessage {
		r, _ := c.NewReceiverForSubscription("routed", sub, &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
		defer r.Close(ctx)
		ms, _ := r.ReceiveMessages(ctxT(t, 3*time.Second), 10, nil)
		return ms
	}
	red := got("red")
	if len(red) != 1 || string(red[0].Body) != "m1" || red[0].ApplicationProperties["routed"] != "yes" {
		t.Fatalf("red subscription: %+v", red)
	}
	corr := got("corr")
	if len(corr) != 1 || string(corr[0].Body) != "m4" {
		t.Fatalf("correlation subscription: %d messages", len(corr))
	}
	if all := got("all"); len(all) != 5 {
		t.Fatalf("default rule subscription got %d", len(all))
	}
}

func TestTopic_SubscriptionDeadLetterQueueAndPathOfTheSubQueue(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	a := newAdmin(t, ns)
	a.CreateTopic(ctx, "dltopic", nil)
	a.CreateSubscription(ctx, "dltopic", "sub", nil)
	c := newClient(t, ns)
	send(t, c, "dltopic", text("bad"))
	r, _ := c.NewReceiverForSubscription("dltopic", "sub", nil)
	defer r.Close(ctx)
	m := recvOne(t, r)
	r.DeadLetterMessage(ctx, m, &azservicebus.DeadLetterOptions{Reason: ptr("nope")})
	dlq, err := c.NewReceiverForSubscription("dltopic", "sub", &azservicebus.ReceiverOptions{SubQueue: azservicebus.SubQueueDeadLetter})
	if err != nil {
		t.Fatal(err)
	}
	defer dlq.Close(ctx)
	dm := recvOne(t, dlq)
	if string(dm.Body) != "bad" || *dm.DeadLetterReason != "nope" {
		t.Fatalf("%+v", dm)
	}
}

func TestTopic_SessionEnabledSubscriptionDeliversPerSession(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	a := newAdmin(t, ns)
	a.CreateTopic(ctx, "sesstopic", nil)
	a.CreateSubscription(ctx, "sesstopic", "sub", &admin.CreateSubscriptionOptions{Properties: &admin.SubscriptionProperties{RequiresSession: ptr(true)}})
	c := newClient(t, ns)
	send(t, c, "sesstopic", sessMsg("k1", "a"), sessMsg("k2", "b"), sessMsg("k1", "c"))
	sr, err := c.AcceptSessionForSubscription(ctxT(t, 20*time.Second), "sesstopic", "sub", "k1", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer sr.Close(ctx)
	ms, _ := sr.ReceiveMessages(ctxT(t, 5*time.Second), 5, nil)
	if len(ms) != 2 || string(ms[0].Body) != "a" || string(ms[1].Body) != "c" {
		t.Fatalf("%d messages", len(ms))
	}
}
