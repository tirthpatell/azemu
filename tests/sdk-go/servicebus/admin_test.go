package servicebus

import (
	"context"
	"fmt"
	"net/http"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus/admin"
)

func TestAdmin_CreateGetUpdateDeleteQueueRoundTripsProperties(t *testing.T) {
	ctx := context.Background()
	a := newAdmin(t, newNS())
	_, err := a.CreateQueue(ctx, "adm-q", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{
		LockDuration: ptr("PT45S"), MaxDeliveryCount: ptr(int32(7)), DefaultMessageTimeToLive: ptr("PT2H"),
		DeadLetteringOnMessageExpiration: ptr(true), RequiresSession: ptr(true), UserMetadata: ptr("meta"), MaxSizeInMegabytes: ptr(int32(2048)),
	}})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	got, err := a.GetQueue(ctx, "adm-q", nil)
	if err != nil || got == nil {
		t.Fatalf("get: %v", err)
	}
	if *got.LockDuration != "PT45S" || *got.MaxDeliveryCount != 7 || *got.DefaultMessageTimeToLive != "PT2H" || !*got.DeadLetteringOnMessageExpiration || !*got.RequiresSession || *got.UserMetadata != "meta" || *got.MaxSizeInMegabytes != 2048 {
		t.Fatalf("properties: %+v", got.QueueProperties)
	}
	props := got.QueueProperties
	props.MaxDeliveryCount = ptr(int32(3))
	if _, err := a.UpdateQueue(ctx, "adm-q", props, nil); err != nil {
		t.Fatalf("update: %v", err)
	}
	got, _ = a.GetQueue(ctx, "adm-q", nil)
	if *got.MaxDeliveryCount != 3 {
		t.Fatalf("after update %d", *got.MaxDeliveryCount)
	}
	if _, err := a.DeleteQueue(ctx, "adm-q", nil); err != nil {
		t.Fatal(err)
	}
	missing, err := a.GetQueue(ctx, "adm-q", nil)
	if err != nil || missing != nil {
		t.Fatalf("GetQueue of a deleted queue returns nil, nil in this SDK; got %v %v", missing, err)
	}
}

func TestAdmin_CreateExistingQueueIsConflict409(t *testing.T) {
	ctx := context.Background()
	a := newAdmin(t, newNS())
	if _, err := a.CreateQueue(ctx, "dupq", nil); err != nil {
		t.Fatal(err)
	}
	_, err := a.CreateQueue(ctx, "dupq", nil)
	var re *azcore.ResponseError
	if !asRE(err, &re) || re.StatusCode != http.StatusConflict {
		t.Fatalf("expected 409, got %v", err)
	}
}

func TestAdmin_ListQueuesPagesThroughAllOfThem(t *testing.T) {
	ctx := context.Background()
	a := newAdmin(t, newNS())
	for i := 0; i < 7; i++ {
		if _, err := a.CreateQueue(ctx, fmt.Sprintf("lq-%02d", i), nil); err != nil {
			t.Fatal(err)
		}
	}
	n := 0
	for p := a.NewListQueuesPager(&admin.ListQueuesOptions{MaxPageSize: 3}); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		n += len(pg.Queues)
	}
	if n != 7 {
		t.Fatalf("listed %d", n)
	}
}

func TestAdmin_RuntimePropertiesCountActiveDeadLetteredAndScheduledMessages(t *testing.T) {
	ctx := context.Background()
	ns := newNS()
	a := newAdmin(t, ns)
	a.CreateQueue(ctx, "rt-q", nil)
	c := newClient(t, ns)
	send(t, c, "rt-q", text("1"), text("2"), text("3"))
	s, _ := c.NewSender("rt-q", nil)
	s.ScheduleMessages(ctx, []*azservicebus.Message{text("later")}, time.Now().Add(time.Hour), nil)
	s.Close(ctx)
	r := newReceiver(t, c, "rt-q", nil)
	m := recvOne(t, r)
	r.DeadLetterMessage(ctx, m, nil)
	rp, err := a.GetQueueRuntimeProperties(ctx, "rt-q", nil)
	if err != nil {
		t.Fatal(err)
	}
	if rp.ActiveMessageCount != 2 || rp.DeadLetterMessageCount != 1 || rp.ScheduledMessageCount != 1 {
		t.Fatalf("active %d dead %d scheduled %d", rp.ActiveMessageCount, rp.DeadLetterMessageCount, rp.ScheduledMessageCount)
	}
	if rp.SizeInBytes <= 0 || rp.CreatedAt.IsZero() {
		t.Fatalf("size %d created %v", rp.SizeInBytes, rp.CreatedAt)
	}
}

func TestAdmin_NamespacePropertiesAnswer(t *testing.T) {
	a := newAdmin(t, newNS())
	p, err := a.GetNamespaceProperties(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if p.Name == "" {
		t.Fatalf("namespace properties %+v", p)
	}
}

func TestAdmin_TopicSubscriptionAndRulesCrud(t *testing.T) {
	ctx := context.Background()
	a := newAdmin(t, newNS())
	if _, err := a.CreateTopic(ctx, "t1", &admin.CreateTopicOptions{Properties: &admin.TopicProperties{RequiresDuplicateDetection: ptr(true), DuplicateDetectionHistoryTimeWindow: ptr("PT1M")}}); err != nil {
		t.Fatal(err)
	}
	if _, err := a.CreateSubscription(ctx, "t1", "s1", &admin.CreateSubscriptionOptions{Properties: &admin.SubscriptionProperties{MaxDeliveryCount: ptr(int32(4)), LockDuration: ptr("PT30S")}}); err != nil {
		t.Fatal(err)
	}
	sub, err := a.GetSubscription(ctx, "t1", "s1", nil)
	if err != nil || *sub.MaxDeliveryCount != 4 || *sub.LockDuration != "PT30S" {
		t.Fatalf("subscription %v %+v", err, sub)
	}
	// the default rule is a true filter
	def, err := a.GetRule(ctx, "t1", "s1", "$Default", nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := def.Filter.(*admin.TrueFilter); !ok {
		t.Fatalf("default rule filter %T", def.Filter)
	}
	a.DeleteRule(ctx, "t1", "s1", "$Default", nil)
	mk := func(name string, f admin.RuleFilter, act admin.RuleAction) {
		t.Helper()
		if _, err := a.CreateRule(ctx, "t1", "s1", &admin.CreateRuleOptions{Name: ptr(name), Filter: f, Action: act}); err != nil {
			t.Fatalf("create rule %s: %v", name, err)
		}
	}
	mk("sql", &admin.SQLFilter{Expression: "color = 'red' AND weight > @w", Parameters: map[string]any{"@w": int64(5)}}, &admin.SQLAction{Expression: "SET seen = 1"})
	mk("corr", &admin.CorrelationFilter{Subject: ptr("sale"), ApplicationProperties: map[string]any{"region": "eu"}}, nil)
	mk("never", &admin.FalseFilter{}, nil)
	got := map[string]admin.RuleProperties{}
	for p := a.NewListRulesPager("t1", "s1", nil); p.More(); {
		pg, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		for _, r := range pg.Rules {
			got[r.Name] = r
		}
	}
	if len(got) != 3 {
		t.Fatalf("rules %v", got)
	}
	sf, ok := got["sql"].Filter.(*admin.SQLFilter)
	if !ok || sf.Expression != "color = 'red' AND weight > @w" || fmt.Sprint(sf.Parameters["@w"]) != "5" {
		t.Fatalf("sql rule %+v", got["sql"].Filter)
	}
	if act, ok := got["sql"].Action.(*admin.SQLAction); !ok || act.Expression != "SET seen = 1" {
		t.Fatalf("action %+v", got["sql"].Action)
	}
	cf, ok := got["corr"].Filter.(*admin.CorrelationFilter)
	if !ok || *cf.Subject != "sale" || cf.ApplicationProperties["region"] != "eu" {
		t.Fatalf("correlation rule %+v", got["corr"].Filter)
	}
	if _, ok := got["never"].Filter.(*admin.FalseFilter); !ok {
		t.Fatalf("false rule %T", got["never"].Filter)
	}
	// update a rule
	r := got["sql"]
	r.Filter = &admin.SQLFilter{Expression: "color = 'blue'"}
	if _, err := a.UpdateRule(ctx, "t1", "s1", r); err != nil {
		t.Fatalf("update rule: %v", err)
	}
	upd, _ := a.GetRule(ctx, "t1", "s1", "sql", nil)
	if upd.Filter.(*admin.SQLFilter).Expression != "color = 'blue'" {
		t.Fatalf("after update %+v", upd.Filter)
	}
	if _, err := a.DeleteSubscription(ctx, "t1", "s1", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := a.DeleteTopic(ctx, "t1", nil); err != nil {
		t.Fatal(err)
	}
}

func TestAdmin_InvalidLockDurationIsBadRequest400(t *testing.T) {
	a := newAdmin(t, newNS())
	_, err := a.CreateQueue(context.Background(), "badlock", &admin.CreateQueueOptions{Properties: &admin.QueueProperties{LockDuration: ptr("PT10M")}})
	var re *azcore.ResponseError
	if !asRE(err, &re) || re.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 for a lock duration over 5 minutes, got %v", err)
	}
}
