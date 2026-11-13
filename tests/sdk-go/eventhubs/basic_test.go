package eventhubs

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azeventhubs/v2"
)

func TestSend_EventToAPartitionIsReadBackFromEarliestWithItsPropertiesAndSystemFields(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub1")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("1")}, &azeventhubs.EventData{
		Body: []byte("hello hub"), ContentType: ptr("text/plain"), MessageID: ptr("m-1"), CorrelationID: "corr-1",
		Properties: map[string]any{"s": "str", "n": int32(5), "b": true},
	})
	c := newConsumer(t, ns, "hub1", "")
	got := receive(t, c, "1", earliest(), 1, 20*time.Second)
	if len(got) != 1 {
		t.Fatalf("received %d events", len(got))
	}
	e := got[0]
	if string(e.Body) != "hello hub" || *e.ContentType != "text/plain" || *e.MessageID != "m-1" || e.CorrelationID != "corr-1" {
		t.Fatalf("event %+v", e.EventData)
	}
	if e.Properties["s"] != "str" || e.Properties["n"] != int32(5) || e.Properties["b"] != true {
		t.Fatalf("properties %#v", e.Properties)
	}
	if e.EnqueuedTime == nil || e.Offset == "" || e.SequenceNumber < 0 {
		t.Fatalf("system fields: enq=%v offset=%q seq=%d", e.EnqueuedTime, e.Offset, e.SequenceNumber)
	}
}

func TestSend_OtherPartitionsDoNotSeeAnEventSentToOne(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub2")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("2")}, ev("only on 2"))
	c := newConsumer(t, ns, "hub2", "")
	if got := receive(t, c, "0", earliest(), 1, 2*time.Second); len(got) != 0 {
		t.Fatalf("partition 0 got %d events", len(got))
	}
	if got := receive(t, c, "2", earliest(), 1, 20*time.Second); len(got) != 1 {
		t.Fatalf("partition 2 got %d events", len(got))
	}
}

func TestSend_BatchWithPartitionKeyAlwaysLandsOnTheSamePartition(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub3")
	for i := 0; i < 3; i++ {
		sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionKey: ptr("customer-42")}, ev(fmt.Sprintf("e%d", i)))
	}
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionKey: ptr("customer-77")}, ev("other"))
	c := newConsumer(t, ns, "hub3", "")
	counts := map[string]int{}
	for _, id := range []string{"0", "1", "2", "3"} {
		for _, e := range receive(t, c, id, earliest(), 10, 2*time.Second) {
			if e.PartitionKey != nil && *e.PartitionKey == "customer-42" {
				counts[id]++
			}
		}
	}
	if len(counts) != 1 {
		t.Fatalf("events of one key spread over partitions: %v", counts)
	}
	for _, n := range counts {
		if n != 3 {
			t.Fatalf("count %v", counts)
		}
	}
}

func TestSend_BatchOfManyEventsKeepsOrderWithinAPartition(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub4")
	var evs []*azeventhubs.EventData
	for i := 0; i < 50; i++ {
		evs = append(evs, ev(fmt.Sprintf("%03d", i)))
	}
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, evs...)
	c := newConsumer(t, ns, "hub4", "")
	got := receive(t, c, "0", earliest(), 50, 20*time.Second)
	if len(got) != 50 {
		t.Fatalf("received %d", len(got))
	}
	for i, e := range got {
		if string(e.Body) != fmt.Sprintf("%03d", i) {
			t.Fatalf("order at %d: %s", i, e.Body)
		}
	}
}

func TestSend_BatchStopsAcceptingEventsWhenFullWithErrEventDataTooLarge(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub5")
	ctx := ctxT(t, 20*time.Second)
	b, err := p.NewEventDataBatch(ctx, &azeventhubs.EventDataBatchOptions{MaxBytes: 2048})
	if err != nil {
		t.Fatal(err)
	}
	var full bool
	for i := 0; i < 100; i++ {
		if err := b.AddEventData(&azeventhubs.EventData{Body: make([]byte, 300)}, nil); err != nil {
			if err == azeventhubs.ErrEventDataTooLarge {
				full = true
				break
			}
			t.Fatal(err)
		}
	}
	if !full {
		t.Fatal("batch never filled up")
	}
	if b.NumEvents() == 0 || b.NumBytes() > 2048 {
		t.Fatalf("batch events %d bytes %d", b.NumEvents(), b.NumBytes())
	}
}

func TestStart_FromLatestSkipsOldEventsAndSeesNewOnes(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub6")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("old"))
	c := newConsumer(t, ns, "hub6", "")
	pc, err := c.NewPartitionClient("0", &azeventhubs.PartitionClientOptions{StartPosition: azeventhubs.StartPosition{Latest: ptr(true)}})
	if err != nil {
		t.Fatal(err)
	}
	defer pc.Close(context.Background())
	// link must be established before the new event is sent: ReceiveEvents attaches on first call
	done := make(chan []*azeventhubs.ReceivedEventData, 1)
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		defer cancel()
		evs, _ := pc.ReceiveEvents(ctx, 1, nil)
		done <- evs
	}()
	time.Sleep(1500 * time.Millisecond)
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("new"))
	evs := <-done
	if len(evs) != 1 || string(evs[0].Body) != "new" {
		t.Fatalf("got %d events", len(evs))
	}
}

func TestStart_FromASequenceNumberInclusiveAndExclusive(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub7")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("a"), ev("b"), ev("c"), ev("d"))
	c := newConsumer(t, ns, "hub7", "")
	all := receive(t, c, "0", earliest(), 4, 20*time.Second)
	if len(all) != 4 {
		t.Fatalf("%d", len(all))
	}
	seq := all[1].SequenceNumber
	excl := receive(t, c, "0", azeventhubs.StartPosition{SequenceNumber: &seq}, 5, 3*time.Second)
	if len(excl) != 2 || string(excl[0].Body) != "c" {
		t.Fatalf("exclusive: %d events, first %q", len(excl), func() string {
			if len(excl) > 0 {
				return string(excl[0].Body)
			}
			return ""
		}())
	}
	incl := receive(t, c, "0", azeventhubs.StartPosition{SequenceNumber: &seq, Inclusive: true}, 5, 3*time.Second)
	if len(incl) != 3 || string(incl[0].Body) != "b" {
		t.Fatalf("inclusive: %d events", len(incl))
	}
}

func TestStart_FromAnOffsetAndFromAnEnqueuedTime(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub8")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("before"))
	time.Sleep(1200 * time.Millisecond)
	cut := time.Now().UTC()
	time.Sleep(1200 * time.Millisecond)
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("after1"), ev("after2"))
	c := newConsumer(t, ns, "hub8", "")
	byTime := receive(t, c, "0", azeventhubs.StartPosition{EnqueuedTime: &cut}, 5, 3*time.Second)
	if len(byTime) != 2 || string(byTime[0].Body) != "after1" {
		t.Fatalf("by enqueued time: %d events", len(byTime))
	}
	all := receive(t, c, "0", earliest(), 3, 10*time.Second)
	off := all[0].Offset
	byOffset := receive(t, c, "0", azeventhubs.StartPosition{Offset: &off}, 5, 3*time.Second)
	if len(byOffset) != 2 || string(byOffset[0].Body) != "after1" && string(byOffset[0].Body) != "before" {
		t.Fatalf("by offset: %d events", len(byOffset))
	}
}

func TestProperties_EventHubPropertiesListFourPartitionsAndTheName(t *testing.T) {
	ns := newNS()
	skipIfNoGeoReplicationFactor(t, ns, "hub9")
	p := newProducer(t, ns, "hub9")
	hp, err := p.GetEventHubProperties(ctxT(t, 20*time.Second), nil)
	if err != nil {
		t.Fatal(err)
	}
	if hp.Name != "hub9" || len(hp.PartitionIDs) != 4 || hp.GeoReplicationEnabled || hp.CreatedOn.IsZero() {
		t.Fatalf("hub properties %+v", hp)
	}
}

func TestProperties_PartitionPropertiesFollowTheEventsSent(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub9b")
	empty, err := p.GetPartitionProperties(ctxT(t, 20*time.Second), "0", nil)
	if err != nil {
		t.Fatalf("partition properties: %v", err)
	}
	if !empty.IsEmpty || empty.LastEnqueuedSequenceNumber != -1 {
		t.Fatalf("empty partition %+v", empty)
	}
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("x"), ev("y"), ev("z"))
	pp, err := p.GetPartitionProperties(ctxT(t, 20*time.Second), "0", nil)
	if err != nil {
		t.Fatal(err)
	}
	if pp.IsEmpty || pp.LastEnqueuedSequenceNumber != pp.BeginningSequenceNumber+2 || pp.LastEnqueuedOffset == "" || pp.LastEnqueuedOn.IsZero() || pp.PartitionID != "0" || pp.EventHubName != "hub9b" {
		t.Fatalf("partition after sending %+v", pp)
	}
}

func TestProperties_PartitionPropertiesOfAMissingPartitionIsAnError(t *testing.T) {
	ns := newNS()
	ensureHub(t, ns, "hub9c", "")
	p, err := azeventhubs.NewProducerClientFromConnectionString(connStr(ns), "hub9c", &azeventhubs.ProducerClientOptions{RetryOptions: azeventhubs.RetryOptions{MaxRetries: -1}})
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close(context.Background())
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("x"))
	_, err = p.GetPartitionProperties(ctxT(t, 10*time.Second), "99", nil)
	if err == nil {
		t.Fatal("partition 99 does not exist; the service answers with status 400 and a description naming the partition")
	}
	if !strings.Contains(err.Error(), "99") {
		t.Fatalf("error does not name the partition: %v", err)
	}
}

func TestConsumerGroups_EachGroupReadsEveryEventIndependently(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub10")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("one"), ev("two"))
	for _, g := range []string{"$Default", "analytics", "audit"} {
		c := newConsumer(t, ns, "hub10", g)
		got := receive(t, c, "0", earliest(), 2, 20*time.Second)
		if len(got) != 2 {
			t.Fatalf("group %s got %d", g, len(got))
		}
	}
}

func TestOwnerLevel_AHigherEpochTakesThePartitionAndTheLowerReceiverGetsAnError(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub11")
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("x"))
	low := newConsumer(t, ns, "hub11", "")
	pcLow, err := low.NewPartitionClient("0", &azeventhubs.PartitionClientOptions{StartPosition: earliest(), OwnerLevel: ptr(int64(1))})
	if err != nil {
		t.Fatal(err)
	}
	defer pcLow.Close(context.Background())
	if _, err := pcLow.ReceiveEvents(ctxT(t, 10*time.Second), 1, nil); err != nil {
		t.Fatalf("low receiver: %v", err)
	}
	high := newConsumer(t, ns, "hub11", "")
	pcHigh, _ := high.NewPartitionClient("0", &azeventhubs.PartitionClientOptions{StartPosition: earliest(), OwnerLevel: ptr(int64(5))})
	defer pcHigh.Close(context.Background())
	if _, err := pcHigh.ReceiveEvents(ctxT(t, 10*time.Second), 1, nil); err != nil {
		t.Fatalf("high receiver: %v", err)
	}
	// the low one has been disconnected: it must report an error (ownership lost) instead of waiting for ever
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("later"))
	_, err = pcLow.ReceiveEvents(ctxT(t, 15*time.Second), 1, nil)
	if err == nil {
		t.Fatal("the receiver with the lower owner level must be disconnected")
	}
	var ehErr *azeventhubs.Error
	if asEH(err, &ehErr) && ehErr.Code != azeventhubs.ErrorCodeOwnershipLost {
		t.Logf("code %v: %v", ehErr.Code, err)
	}
}

func TestSend_LargeEventOfHalfAMegabyteCrossesManyAmqpFramesIntact(t *testing.T) {
	ns := newNS()
	p := newProducer(t, ns, "hub12")
	body := make([]byte, 500*1024)
	for i := range body {
		body[i] = byte(i * 3)
	}
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, &azeventhubs.EventData{Body: body})
	c := newConsumer(t, ns, "hub12", "")
	got := receive(t, c, "0", earliest(), 1, 20*time.Second)
	if len(got) != 1 || len(got[0].Body) != len(body) || got[0].Body[123456] != body[123456] {
		t.Fatalf("large event: %d events", len(got))
	}
}
