package servicebus

import (
	"context"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
)

func TestQueue_SendThenReceivePeekLockAndCompleteLeavesTheQueueEmpty(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-basic"))
	q := "q-basic"
	send(t, c, q, &azservicebus.Message{
		Body: []byte("hello"), MessageID: ptr("id-1"), ContentType: ptr("text/plain"), Subject: ptr("subj"), CorrelationID: ptr("corr"),
		To: ptr("dest"), ReplyTo: ptr("reply"), ApplicationProperties: map[string]any{"s": "str", "i": int32(7), "b": true, "f": 1.5, "l": int64(1 << 40)},
	})
	r := newReceiver(t, c, q, nil)
	m := recvOne(t, r)
	if string(m.Body) != "hello" || m.MessageID != "id-1" || *m.ContentType != "text/plain" || *m.Subject != "subj" || *m.CorrelationID != "corr" || *m.To != "dest" || *m.ReplyTo != "reply" {
		t.Fatalf("message %+v", m)
	}
	if m.ApplicationProperties["s"] != "str" || m.ApplicationProperties["i"] != int32(7) || m.ApplicationProperties["b"] != true || m.ApplicationProperties["f"] != 1.5 || m.ApplicationProperties["l"] != int64(1<<40) {
		t.Fatalf("application properties %#v", m.ApplicationProperties)
	}
	if m.DeliveryCount != 1 || m.SequenceNumber == nil || m.EnqueuedTime == nil || m.LockedUntil == nil || m.LockedUntil.Before(time.Now()) {
		t.Fatalf("broker properties: count=%d seq=%v enq=%v lock=%v", m.DeliveryCount, m.SequenceNumber, m.EnqueuedTime, m.LockedUntil)
	}
	if err := r.CompleteMessage(context.Background(), m, nil); err != nil {
		t.Fatal(err)
	}
	peek, err := r.PeekMessages(ctxT(t, 10*time.Second), 5, nil)
	if err != nil || len(peek) != 0 {
		t.Fatalf("peek after complete: %v %d", err, len(peek))
	}
}

func TestQueue_AbandonRedeliversTheMessageWithADeliveryCountOfTwo(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-abandon"))
	send(t, c, "q-abandon", text("again"))
	r := newReceiver(t, c, "q-abandon", nil)
	m := recvOne(t, r)
	if err := r.AbandonMessage(context.Background(), m, &azservicebus.AbandonMessageOptions{PropertiesToModify: map[string]any{"tried": "once"}}); err != nil {
		t.Fatal(err)
	}
	m2 := recvOne(t, r)
	if string(m2.Body) != "again" || m2.DeliveryCount != 2 {
		t.Fatalf("redelivery: %q count %d", m2.Body, m2.DeliveryCount)
	}
	if m2.ApplicationProperties["tried"] != "once" {
		t.Fatalf("modified properties %#v", m2.ApplicationProperties)
	}
	r.CompleteMessage(context.Background(), m2, nil)
}

func TestQueue_DeadLetterWithReasonAndDescriptionLandsInTheDeadLetterSubQueue(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-dlq"))
	send(t, c, "q-dlq", text("poison"))
	r := newReceiver(t, c, "q-dlq", nil)
	m := recvOne(t, r)
	if err := r.DeadLetterMessage(context.Background(), m, &azservicebus.DeadLetterOptions{Reason: ptr("bad"), ErrorDescription: ptr("it is poison")}); err != nil {
		t.Fatal(err)
	}
	dlq := newReceiver(t, c, "q-dlq", &azservicebus.ReceiverOptions{SubQueue: azservicebus.SubQueueDeadLetter})
	dm := recvOne(t, dlq)
	if string(dm.Body) != "poison" || dm.DeadLetterReason == nil || *dm.DeadLetterReason != "bad" || dm.DeadLetterErrorDescription == nil || *dm.DeadLetterErrorDescription != "it is poison" {
		t.Fatalf("dead letter: %+v", dm)
	}
	dlq.CompleteMessage(context.Background(), dm, nil)
}

func TestQueue_DeferredMessageIsNotReceivedNormallyButCanBeFetchedBySequenceNumber(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-defer"))
	send(t, c, "q-defer", text("first"), text("second"))
	r := newReceiver(t, c, "q-defer", nil)
	ms, err := r.ReceiveMessages(ctxT(t, 10*time.Second), 2, nil)
	if err != nil || len(ms) != 2 {
		t.Fatalf("receive: %v %d", err, len(ms))
	}
	deferred := ms[0]
	if err := r.DeferMessage(context.Background(), deferred, nil); err != nil {
		t.Fatal(err)
	}
	r.CompleteMessage(context.Background(), ms[1], nil)
	none, _ := r.ReceiveMessages(ctxT(t, 2*time.Second), 1, nil)
	if len(none) != 0 {
		t.Fatal("a deferred message must not be delivered by a normal receive")
	}
	got, err := r.ReceiveDeferredMessages(ctxT(t, 10*time.Second), []int64{*deferred.SequenceNumber}, nil)
	if err != nil || len(got) != 1 || string(got[0].Body) != "first" {
		t.Fatalf("deferred fetch: %v %v", err, got)
	}
	if got[0].State != azservicebus.MessageStateDeferred {
		t.Fatalf("state %v", got[0].State)
	}
	r.CompleteMessage(context.Background(), got[0], nil)
}

func TestQueue_ReceiveAndDeleteModeRemovesTheMessageAtOnce(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-rad"))
	send(t, c, "q-rad", text("gone"))
	r := newReceiver(t, c, "q-rad", &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
	m := recvOne(t, r)
	if string(m.Body) != "gone" {
		t.Fatal(string(m.Body))
	}
	r2 := newReceiver(t, c, "q-rad", nil)
	more, _ := r2.ReceiveMessages(ctxT(t, 2*time.Second), 1, nil)
	if len(more) != 0 {
		t.Fatal("message still in the queue")
	}
}

func TestQueue_ReceiveMessagesReturnsWhatIsThereWhenTheContextEnds(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-partial"))
	send(t, c, "q-partial", text("1"), text("2"), text("3"))
	r := newReceiver(t, c, "q-partial", nil)
	start := time.Now()
	ms, err := r.ReceiveMessages(ctxT(t, 3*time.Second), 10, nil)
	if err != nil {
		t.Fatalf("receive: %v", err)
	}
	if len(ms) != 3 {
		t.Fatalf("got %d messages", len(ms))
	}
	if time.Since(start) > 10*time.Second {
		t.Fatal("took too long")
	}
}

func TestQueue_PeekDoesNotLockAndCanStartFromASequenceNumber(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-peek"))
	send(t, c, "q-peek", text("a"), text("b"), text("c"))
	r := newReceiver(t, c, "q-peek", nil)
	p1, err := r.PeekMessages(ctxT(t, 10*time.Second), 10, nil)
	if err != nil || len(p1) != 3 {
		t.Fatalf("peek: %v %d", err, len(p1))
	}
	for _, m := range p1 {
		if m.LockedUntil != nil && !m.LockedUntil.IsZero() && m.LockedUntil.After(time.Now()) {
			t.Fatal("a peeked message must not be locked")
		}
	}
	p2, err := r.PeekMessages(ctxT(t, 10*time.Second), 10, &azservicebus.PeekMessagesOptions{FromSequenceNumber: ptr(*p1[1].SequenceNumber)})
	if err != nil || len(p2) != 2 || string(p2[0].Body) != "b" {
		t.Fatalf("peek from sequence number: %v %d", err, len(p2))
	}
	// still receivable
	ms, _ := r.ReceiveMessages(ctxT(t, 5*time.Second), 3, nil)
	if len(ms) != 3 {
		t.Fatalf("received %d after peek", len(ms))
	}
}

func TestQueue_BatchSendDeliversEveryMessageInOrderAndStopsWhenTheBatchIsFull(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-batch"))
	s, _ := c.NewSender("q-batch", nil)
	defer s.Close(context.Background())
	ctx := ctxT(t, 30*time.Second)
	batch, err := s.NewMessageBatch(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 20; i++ {
		if err := batch.AddMessage(text(string(rune('a'+i))), nil); err != nil {
			t.Fatal(err)
		}
	}
	if batch.NumMessages() != 20 {
		t.Fatalf("batch has %d", batch.NumMessages())
	}
	if err := s.SendMessageBatch(ctx, batch, nil); err != nil {
		t.Fatalf("send batch: %v", err)
	}
	r := newReceiver(t, c, "q-batch", &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
	ms, err := r.ReceiveMessages(ctxT(t, 10*time.Second), 20, nil)
	if err != nil || len(ms) != 20 {
		t.Fatalf("received %d %v", len(ms), err)
	}
	for i, m := range ms {
		if string(m.Body) != string(rune('a'+i)) {
			t.Fatalf("order at %d: %s", i, m.Body)
		}
	}
	// a batch of a small maximum size refuses a bigger message
	small, _ := s.NewMessageBatch(ctx, &azservicebus.MessageBatchOptions{MaxBytes: 1024})
	err = small.AddMessage(&azservicebus.Message{Body: make([]byte, 4096)}, nil)
	if err != azservicebus.ErrMessageTooLarge {
		t.Fatalf("expected ErrMessageTooLarge, got %v", err)
	}
}

func TestQueue_BatchOfThousandMessagesFitsInSeveralBatchesAndAllArrive(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-many"))
	s, _ := c.NewSender("q-many", nil)
	defer s.Close(context.Background())
	ctx := ctxT(t, 60*time.Second)
	sent := 0
	batch, _ := s.NewMessageBatch(ctx, nil)
	for i := 0; i < 1000; i++ {
		m := &azservicebus.Message{Body: make([]byte, 700)}
		if err := batch.AddMessage(m, nil); err == azservicebus.ErrMessageTooLarge {
			if err := s.SendMessageBatch(ctx, batch, nil); err != nil {
				t.Fatal(err)
			}
			sent += int(batch.NumMessages())
			batch, _ = s.NewMessageBatch(ctx, nil)
			if err := batch.AddMessage(m, nil); err != nil {
				t.Fatal(err)
			}
		} else if err != nil {
			t.Fatal(err)
		}
	}
	sent += int(batch.NumMessages())
	if err := s.SendMessageBatch(ctx, batch, nil); err != nil {
		t.Fatal(err)
	}
	if sent != 1000 {
		t.Fatalf("sent %d", sent)
	}
	r := newReceiver(t, c, "q-many", &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
	got := 0
	for got < 1000 {
		ms, err := r.ReceiveMessages(ctxT(t, 20*time.Second), 500, nil)
		if err != nil {
			t.Fatal(err)
		}
		if len(ms) == 0 {
			break
		}
		got += len(ms)
	}
	if got != 1000 {
		t.Fatalf("received %d of 1000", got)
	}
}

func TestQueue_LargeMessageOfTwoHundredKiBCrossesManyAmqpFramesIntact(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-large"))
	body := make([]byte, 200*1024)
	for i := range body {
		body[i] = byte(i * 7)
	}
	send(t, c, "q-large", &azservicebus.Message{Body: body})
	r := newReceiver(t, c, "q-large", nil)
	m := recvOne(t, r)
	if len(m.Body) != len(body) {
		t.Fatalf("size %d", len(m.Body))
	}
	for i := range body {
		if m.Body[i] != body[i] {
			t.Fatalf("byte %d differs", i)
		}
	}
	r.CompleteMessage(context.Background(), m, nil)
}

func TestQueue_MessageOverTheSizeLimitIsRefused(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-toobig"))
	s, _ := c.NewSender("q-toobig", nil)
	defer s.Close(context.Background())
	err := s.SendMessage(ctxT(t, 20*time.Second), &azservicebus.Message{Body: make([]byte, 2*1024*1024)}, nil)
	if err == nil {
		t.Fatal("a 2 MiB message must be refused (limit 256 KiB)")
	}
}

func TestQueue_ScheduledMessageIsHiddenUntilItsTimeAndCanBeCancelled(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-sched"))
	s, _ := c.NewSender("q-sched", nil)
	defer s.Close(context.Background())
	ctx := ctxT(t, 40*time.Second)
	when := time.Now().Add(3 * time.Second)
	seq, err := s.ScheduleMessages(ctx, []*azservicebus.Message{text("later")}, when, nil)
	if err != nil || len(seq) != 1 {
		t.Fatalf("schedule: %v %v", err, seq)
	}
	r := newReceiver(t, c, "q-sched", nil)
	early, _ := r.ReceiveMessages(ctxT(t, 1500*time.Millisecond), 1, nil)
	if len(early) != 0 {
		t.Fatal("scheduled message arrived early")
	}
	m := recvOne(t, r)
	if string(m.Body) != "later" || time.Since(when) < -500*time.Millisecond {
		t.Fatalf("message %q at %v", m.Body, time.Since(when))
	}
	r.CompleteMessage(ctx, m, nil)
	// cancel one
	seq2, _ := s.ScheduleMessages(ctx, []*azservicebus.Message{text("never")}, time.Now().Add(2*time.Second), nil)
	if err := s.CancelScheduledMessages(ctx, seq2, nil); err != nil {
		t.Fatalf("cancel: %v", err)
	}
	none, _ := r.ReceiveMessages(ctxT(t, 3500*time.Millisecond), 1, nil)
	if len(none) != 0 {
		t.Fatal("cancelled message was delivered")
	}
}

func TestQueue_MessageWithTimeToLiveExpiresAndIsGone(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-ttl"))
	send(t, c, "q-ttl", &azservicebus.Message{Body: []byte("short"), TimeToLive: ptr(time.Second)})
	time.Sleep(2200 * time.Millisecond)
	r := newReceiver(t, c, "q-ttl", nil)
	ms, _ := r.ReceiveMessages(ctxT(t, 2*time.Second), 1, nil)
	if len(ms) != 0 {
		t.Fatal("expired message delivered")
	}
}

func TestQueue_CompetingReceiversTogetherGetEveryMessageExactlyOnce(t *testing.T) {
	c := newClient(t, nsWithQueue(t, "q-compete"))
	const n = 60
	s, _ := c.NewSender("q-compete", nil)
	ctx := ctxT(t, 60*time.Second)
	batch, _ := s.NewMessageBatch(ctx, nil)
	for i := 0; i < n; i++ {
		batch.AddMessage(&azservicebus.Message{Body: []byte{byte(i)}, MessageID: ptr(string(rune('A' + i)))}, nil)
	}
	s.SendMessageBatch(ctx, batch, nil)
	s.Close(ctx)
	type res struct{ ids []string }
	out := make(chan res, 3)
	for k := 0; k < 3; k++ {
		go func() {
			r, err := c.NewReceiverForQueue("q-compete", nil)
			if err != nil {
				out <- res{}
				return
			}
			defer r.Close(context.Background())
			var ids []string
			for {
				ms, err := r.ReceiveMessages(ctxT(t, 3*time.Second), 5, nil)
				if err != nil || len(ms) == 0 {
					break
				}
				for _, m := range ms {
					ids = append(ids, m.MessageID)
					r.CompleteMessage(context.Background(), m, nil)
				}
			}
			out <- res{ids}
		}()
	}
	seen := map[string]int{}
	for k := 0; k < 3; k++ {
		for _, id := range (<-out).ids {
			seen[id]++
		}
	}
	if len(seen) != n {
		t.Fatalf("distinct messages %d, want %d", len(seen), n)
	}
	for id, c := range seen {
		if c != 1 {
			t.Fatalf("message %s delivered %d times", id, c)
		}
	}
}
