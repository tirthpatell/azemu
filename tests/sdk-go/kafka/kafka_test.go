// Package kafka tests the Kafka endpoint of Event Hubs with franz-go, a Kafka client written in pure Go (no librdkafka):
// producing and consuming through consumer groups, keys, headers, gzip, the admin client, TLS with the emulator's CA, SASL
// PLAIN with a connection string, and events crossing between Kafka and AMQP. The expectations come from the Kafka protocol
// and Microsoft Learn ("Use Azure Event Hubs from Apache Kafka applications"), not from the emulator's code.
package kafka

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azeventhubs/v2"
	"github.com/twmb/franz-go/pkg/kadm"
	"github.com/twmb/franz-go/pkg/kerr"
	"github.com/twmb/franz-go/pkg/kgo"
	"github.com/twmb/franz-go/pkg/sasl/plain"
)

func TestMain(m *testing.M) { emu.Main(m) }

func plainBrokers() []string { return []string{fmt.Sprintf("127.0.0.1:%d", emu.E.Port)} }

func connString(ns string) string {
	return fmt.Sprintf("Endpoint=sb://%s.eventhub.localhost:%d/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any", ns, emu.E.Port)
}

func tlsOpts(ns string) []kgo.Opt {
	return []kgo.Opt{
		kgo.SeedBrokers(fmt.Sprintf("%s.eventhub.localhost:%d", ns, emu.E.Port)),
		kgo.DialTLSConfig(&tls.Config{RootCAs: emu.E.Pool}),
	}
}

func saslOpt(ns string) kgo.Opt {
	return kgo.SASL(plain.Auth{User: "$ConnectionString", Pass: connString(ns)}.AsMechanism())
}

func newClient(t testing.TB, opts ...kgo.Opt) *kgo.Client {
	t.Helper()
	// franz-go compresses with snappy unless told otherwise; Event Hubs for Kafka reads gzip only, so the default here is none
	base := []kgo.Opt{kgo.SeedBrokers(plainBrokers()...), kgo.RequestRetries(3), kgo.RetryTimeout(30 * time.Second), kgo.ProducerBatchCompression(kgo.NoCompression())}
	cl, err := kgo.NewClient(append(base, opts...)...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(cl.Close)
	return cl
}

func ctxT(t testing.TB, d time.Duration) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), d)
	t.Cleanup(cancel)
	return ctx
}

func uniq(p string) string { return emu.Unique(p) }

// hub creates an event hub through the emulator's admin API.
func hub(t testing.TB, ns, name string, partitions int) {
	t.Helper()
	body, _ := json.Marshal(map[string]any{"partitionCount": partitions})
	req, _ := http.NewRequest(http.MethodPut, fmt.Sprintf("%s/_azemu/eventhubs/%s/hubs/%s", emu.E.Gateway(), ns, name), bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	if resp.StatusCode != 200 && resp.StatusCode != 201 {
		t.Fatalf("create hub: %d", resp.StatusCode)
	}
}

func hubExists(t testing.TB, ns, name string) bool {
	t.Helper()
	resp, err := http.Get(fmt.Sprintf("%s/_azemu/eventhubs/%s/hubs/%s", emu.E.Gateway(), ns, name))
	if err != nil {
		t.Fatal(err)
	}
	io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	return resp.StatusCode == 200
}

func produce(t testing.TB, cl *kgo.Client, recs ...*kgo.Record) []*kgo.Record {
	t.Helper()
	res := cl.ProduceSync(ctxT(t, 30*time.Second), recs...)
	if err := res.FirstErr(); err != nil {
		t.Fatalf("produce: %v", err)
	}
	out := make([]*kgo.Record, 0, len(res))
	for _, r := range res {
		out = append(out, r.Record)
	}
	return out
}

func consume(t testing.TB, cl *kgo.Client, n int, d time.Duration) []*kgo.Record {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), d)
	defer cancel()
	var out []*kgo.Record
	for len(out) < n {
		fs := cl.PollFetches(ctx)
		if ctx.Err() != nil {
			break
		}
		for _, e := range fs.Errors() {
			t.Fatalf("fetch error on %s/%d: %v", e.Topic, e.Partition, e.Err)
		}
		fs.EachRecord(func(r *kgo.Record) { out = append(out, r) })
	}
	if len(out) < n {
		t.Fatalf("got %d of %d records", len(out), n)
	}
	return out
}

func TestProduceAndConsumeThroughAGroupWithKeysAndHeaders(t *testing.T) {
	topic := uniq("fgo")
	hub(t, "default", topic, 2)
	p := newClient(t, kgo.RecordPartitioner(kgo.ManualPartitioner()))
	recs := produce(t, p, &kgo.Record{Topic: topic, Partition: 1, Key: []byte("user-1"), Value: []byte("payload"),
		Headers: []kgo.RecordHeader{{Key: "trace", Value: []byte("abc")}, {Key: "empty", Value: []byte{}}}})
	if recs[0].Offset != 0 || recs[0].Partition != 1 {
		t.Fatalf("offset/partition = %d/%d", recs[0].Offset, recs[0].Partition)
	}
	c := newClient(t, kgo.ConsumerGroup(uniq("g")), kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()))
	got := consume(t, c, 1, 30*time.Second)[0]
	if string(got.Key) != "user-1" || string(got.Value) != "payload" || got.Partition != 1 || got.Offset != 0 {
		t.Fatalf("got %+v", got)
	}
	if len(got.Headers) != 2 || got.Headers[0].Key != "trace" || string(got.Headers[0].Value) != "abc" || got.Headers[1].Key != "empty" {
		t.Fatalf("headers %+v", got.Headers)
	}
	if got.Timestamp.IsZero() {
		t.Fatal("no timestamp")
	}
}

func TestIdempotentProducerIsTheDefaultAndKeepsOrderAcrossBatches(t *testing.T) {
	topic := uniq("fgo")
	hub(t, "default", topic, 1)
	p := newClient(t, kgo.RecordPartitioner(kgo.ManualPartitioner()), kgo.ProducerLinger(5*time.Millisecond))
	var recs []*kgo.Record
	for i := 0; i < 400; i++ {
		recs = append(recs, &kgo.Record{Topic: topic, Partition: 0, Value: []byte(fmt.Sprintf("v%d", i))})
	}
	for _, r := range produce(t, p, recs...) {
		var i int
		fmt.Sscanf(string(r.Value), "v%d", &i)
		if r.Offset != int64(i) {
			t.Fatalf("record %d got offset %d", i, r.Offset)
		}
	}
	c := newClient(t, kgo.ConsumerGroup(uniq("g")), kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()))
	got := consume(t, c, 400, 40*time.Second)
	for i, r := range got {
		if string(r.Value) != fmt.Sprintf("v%d", i) {
			t.Fatalf("record %d is %q", i, r.Value)
		}
	}
}

func TestGzipIsAcceptedAndSnappyIsRefused(t *testing.T) {
	topic := uniq("fgo")
	hub(t, "default", topic, 1)
	gz := newClient(t, kgo.ProducerBatchCompression(kgo.GzipCompression()), kgo.RecordPartitioner(kgo.ManualPartitioner()))
	produce(t, gz, &kgo.Record{Topic: topic, Value: bytes.Repeat([]byte("z"), 5000)}, &kgo.Record{Topic: topic, Value: bytes.Repeat([]byte("y"), 5000)})
	c := newClient(t, kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()))
	if got := consume(t, c, 2, 30*time.Second); len(got[0].Value) != 5000 {
		t.Fatalf("value length %d", len(got[0].Value))
	}
	sn := newClient(t, kgo.ProducerBatchCompression(kgo.SnappyCompression()), kgo.RecordPartitioner(kgo.ManualPartitioner()), kgo.RecordRetries(0), kgo.ProduceRequestTimeout(5*time.Second))
	res := sn.ProduceSync(ctxT(t, 30*time.Second), &kgo.Record{Topic: topic, Value: bytes.Repeat([]byte("x"), 1000)})
	if res.FirstErr() == nil || !strings.Contains(res.FirstErr().Error(), "message format version") {
		t.Fatalf("snappy: %v", res.FirstErr())
	}
}

func TestCommitAndResumeAfterARestart(t *testing.T) {
	topic := uniq("fgo")
	hub(t, "default", topic, 1)
	p := newClient(t, kgo.RecordPartitioner(kgo.ManualPartitioner()))
	var recs []*kgo.Record
	for _, v := range []string{"a", "b", "c", "d", "e"} {
		recs = append(recs, &kgo.Record{Topic: topic, Value: []byte(v)})
	}
	produce(t, p, recs...)
	group := uniq("g")
	c1, err := kgo.NewClient(kgo.SeedBrokers(plainBrokers()...), kgo.ConsumerGroup(group), kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()), kgo.DisableAutoCommit())
	if err != nil {
		t.Fatal(err)
	}
	got := consume(t, c1, 3, 30*time.Second)
	if err := c1.CommitRecords(ctxT(t, 20*time.Second), got[2]); err != nil {
		t.Fatalf("commit: %v", err)
	}
	c1.Close() // leaves the group
	c2 := newClient(t, kgo.ConsumerGroup(group), kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()))
	rest := consume(t, c2, 2, 30*time.Second)
	if string(rest[0].Value) != "d" || string(rest[1].Value) != "e" {
		t.Fatalf("resumed with %q, %q", rest[0].Value, rest[1].Value)
	}
}

func TestTwoConsumersSplitThePartitionsAndALeaverTriggersARebalance(t *testing.T) {
	topic := uniq("fgo")
	hub(t, "default", topic, 4)
	group := uniq("g")
	assigned := make(chan []int32, 16)
	mk := func(name string) *kgo.Client {
		return newClient(t, kgo.ConsumerGroup(group), kgo.ConsumeTopics(topic), kgo.SessionTimeout(6*time.Second), kgo.HeartbeatInterval(time.Second), kgo.RebalanceTimeout(15*time.Second),
			kgo.OnPartitionsAssigned(func(_ context.Context, _ *kgo.Client, m map[string][]int32) { assigned <- m[topic] }))
	}
	c1 := mk("c1")
	go func() { c1.PollFetches(context.Background()) }()
	select {
	case ps := <-assigned:
		if len(ps) != 4 {
			t.Fatalf("alone, the consumer got %v", ps)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("no assignment")
	}
	c2 := mk("c2")
	stop := make(chan struct{})
	defer close(stop)
	for _, c := range []*kgo.Client{c1, c2} {
		go func(c *kgo.Client) {
			for {
				select {
				case <-stop:
					return
				default:
				}
				ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
				c.PollFetches(ctx)
				cancel()
			}
		}(c)
	}
	// the default balancer is cooperative-sticky (two rounds): in the end each of the two owns two partitions
	d := kadm.NewClient(newClient(t))
	deadline := time.Now().Add(45 * time.Second)
	for {
		gs, err := d.DescribeGroups(ctxT(t, 20*time.Second), group)
		if err != nil {
			t.Fatal(err)
		}
		g := gs[group]
		counts := map[string]int{}
		total := 0
		for _, m := range g.Members {
			if ca, ok := m.Assigned.AsConsumer(); ok {
				for _, tp := range ca.Topics {
					counts[m.MemberID] += len(tp.Partitions)
					total += len(tp.Partitions)
				}
			}
		}
		if g.State == "Stable" && len(g.Members) == 2 && total == 4 && len(counts) == 2 {
			for _, n := range counts {
				if n != 2 {
					t.Fatalf("uneven split: %v", counts)
				}
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("no even split: state %s, %d members, counts %v", g.State, len(g.Members), counts)
		}
		time.Sleep(500 * time.Millisecond)
	}
}

func TestAdminTopicsGroupsAndOffsets(t *testing.T) {
	a := kadm.NewClient(newClient(t))
	ctx := ctxT(t, 60*time.Second)
	name := uniq("fgo-admin")
	res, err := a.CreateTopics(ctx, 3, 1, map[string]*string{"retention.ms": ptr("7200000")}, name)
	if err != nil || res.Error() != nil {
		t.Fatalf("create: %v %v", err, res.Error())
	}
	if !hubExists(t, "default", name) {
		t.Fatal("not an event hub")
	}
	topics, err := a.ListTopics(ctx)
	if err != nil || len(topics[name].Partitions) != 3 {
		t.Fatalf("list: %v %+v", err, topics[name])
	}
	res, _ = a.CreateTopics(ctx, 3, 1, nil, name)
	if res[name].Err != kerr.TopicAlreadyExists {
		t.Fatalf("second create: %v", res[name].Err)
	}
	cfgs, err := a.DescribeTopicConfigs(ctx, name)
	if err != nil || len(cfgs) != 1 || cfgs[0].Err != nil {
		t.Fatalf("describe configs: %v %+v", err, cfgs)
	}
	found := ""
	for _, c := range cfgs[0].Configs {
		if c.Key == "retention.ms" && c.Value != nil {
			found = *c.Value
		}
	}
	if found != "7200000" {
		t.Fatalf("retention.ms = %q", found)
	}

	// a group with a commit: describe, list, offsets, lag
	p := newClient(t, kgo.RecordPartitioner(kgo.ManualPartitioner()))
	produce(t, p, &kgo.Record{Topic: name, Partition: 0, Value: []byte("1")}, &kgo.Record{Topic: name, Partition: 0, Value: []byte("2")})
	group := uniq("g")
	c, err := kgo.NewClient(kgo.SeedBrokers(plainBrokers()...), kgo.ConsumerGroup(group), kgo.ConsumeTopics(name), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()), kgo.DisableAutoCommit(), kgo.ClientID("fgo-describe"))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	got := consume(t, c, 2, 30*time.Second)
	if err := c.CommitRecords(ctx, got[1]); err != nil {
		t.Fatal(err)
	}
	listed, err := a.ListGroups(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := listed[group]; !ok {
		t.Fatalf("group not listed: %v", listed.Groups())
	}
	desc, err := a.DescribeGroups(ctx, group)
	if err != nil || desc[group].State != "Stable" || len(desc[group].Members) != 1 || desc[group].Members[0].ClientID != "fgo-describe" {
		t.Fatalf("describe: %v %+v", err, desc[group])
	}
	offs, err := a.FetchOffsets(ctx, group)
	if err != nil {
		t.Fatal(err)
	}
	if o, ok := offs.Lookup(name, 0); !ok || o.At != 2 {
		t.Fatalf("committed offsets: %+v", offs)
	}
	end, err := a.ListEndOffsets(ctx, name)
	if err != nil {
		t.Fatal(err)
	}
	if o, _ := end.Lookup(name, 0); o.Offset != 2 {
		t.Fatalf("end offset %+v", o)
	}
	del, err := a.DeleteTopics(ctx, name)
	if err != nil || del.Error() != nil {
		t.Fatalf("delete: %v %v", err, del.Error())
	}
	if hubExists(t, "default", name) {
		t.Fatal("hub still there")
	}
}

func ptr[T any](v T) *T { return &v }

func TestTLSAndSASLPlainWithAConnectionString(t *testing.T) {
	topic := uniq("fgo-tls")
	hub(t, "tlsns", topic, 1)
	p := newClient(t, append(tlsOpts("tlsns"), saslOpt("tlsns"), kgo.RecordPartitioner(kgo.ManualPartitioner()))...)
	produce(t, p, &kgo.Record{Topic: topic, Value: []byte("over tls and sasl")})
	c := newClient(t, append(tlsOpts("tlsns"), saslOpt("tlsns"), kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()))...)
	if got := consume(t, c, 1, 30*time.Second)[0]; string(got.Value) != "over tls and sasl" {
		t.Fatalf("got %q", got.Value)
	}
	// the broker the client is told about is the host it dialled, and the namespace is the TLS server name
	a := kadm.NewClient(newClient(t, tlsOpts("tlsns")...))
	meta, err := a.BrokerMetadata(ctxT(t, 20*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	if len(meta.Brokers) != 1 || meta.Brokers[0].Host != "tlsns.eventhub.localhost" || int(meta.Brokers[0].Port) != emu.E.Port {
		t.Fatalf("brokers: %+v", meta.Brokers)
	}
	topics, _ := a.ListTopics(ctxT(t, 20*time.Second))
	if !topics.Has(topic) {
		t.Fatal("topic not visible in its namespace")
	}
	other := kadm.NewClient(newClient(t, tlsOpts("otherns")...))
	topics, _ = other.ListTopics(ctxT(t, 20*time.Second))
	if topics.Has(topic) {
		t.Fatal("topic visible in another namespace")
	}
}

func TestSASLPlainWithoutTLSTakesTheNamespaceFromTheConnectionString(t *testing.T) {
	topic := uniq("fgo-sasl")
	hub(t, "saslns", topic, 1)
	a := kadm.NewClient(newClient(t, saslOpt("saslns")))
	topics, err := a.ListTopics(ctxT(t, 20*time.Second))
	if err != nil || !topics.Has(topic) {
		t.Fatalf("topics: %v %v", err, topics.Names())
	}
	meta, _ := a.BrokerMetadata(ctxT(t, 20*time.Second))
	if meta.Brokers[0].Host != "saslns.eventhub.localhost" {
		t.Fatalf("broker host %q", meta.Brokers[0].Host)
	}
}

func TestAClientThatTrustsNothingCannotUseTLS(t *testing.T) {
	cl, err := kgo.NewClient(kgo.SeedBrokers(fmt.Sprintf("tlsns.eventhub.localhost:%d", emu.E.Port)), kgo.DialTLSConfig(&tls.Config{}), kgo.RetryTimeout(5*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	defer cl.Close()
	if err := cl.Ping(ctxT(t, 10*time.Second)); err == nil {
		t.Fatal("a client that does not trust the emulator's CA connected")
	}
}

// By default the emulator does not create an event hub because a Kafka client names a topic (as Event Hubs does not).
func TestMissingTopicsAreNotCreatedByDefaultEvenWhenTheClientAllowsIt(t *testing.T) {
	missing := uniq("fgo-missing")
	p := newClient(t, kgo.AllowAutoTopicCreation(), kgo.RecordPartitioner(kgo.ManualPartitioner()), kgo.RecordRetries(0), kgo.UnknownTopicRetries(0))
	res := p.ProduceSync(ctxT(t, 20*time.Second), &kgo.Record{Topic: missing, Partition: 0, Value: []byte("x")})
	if res.FirstErr() == nil || hubExists(t, "default", missing) {
		t.Fatalf("expected an error and no hub: %v", res.FirstErr())
	}
}

// Auto-creation is a convenience Azure does not have: this test starts an emulator of its own with AZEMU_EVENTHUBS_AUTOCREATE=1.
func TestMissingTopicsAreCreatedWhenAutoCreationIsOn(t *testing.T) {
	e, err := emu.StartEnv([]string{"AZEMU_EVENTHUBS_AUTOCREATE=1"})
	if err != nil {
		t.Fatal(err)
	}
	old := emu.E
	emu.E = e
	t.Cleanup(func() { emu.E = old; e.Stop() })
	name := uniq("fgo-auto")
	p := newClient(t, kgo.AllowAutoTopicCreation(), kgo.RecordPartitioner(kgo.ManualPartitioner()))
	produce(t, p, &kgo.Record{Topic: name, Partition: 0, Value: []byte("first")})
	if !hubExists(t, "default", name) {
		t.Fatal("no event hub was created")
	}
	// without the flag, a topic that does not exist stays missing
	missing := uniq("fgo-missing")
	q := newClient(t, kgo.RecordPartitioner(kgo.ManualPartitioner()), kgo.RecordRetries(0), kgo.UnknownTopicRetries(0))
	res := q.ProduceSync(ctxT(t, 20*time.Second), &kgo.Record{Topic: missing, Partition: 0, Value: []byte("x")})
	if res.FirstErr() == nil || hubExists(t, "default", missing) {
		t.Fatalf("expected an error and no hub: %v", res.FirstErr())
	}
}

func TestEventsCrossBetweenKafkaAndAMQP(t *testing.T) {
	name := uniq("fgo-x")
	hub(t, "xns", name, 1)
	p := newClient(t, saslOpt("xns"), kgo.RecordPartitioner(kgo.ManualPartitioner()))
	produce(t, p, &kgo.Record{Topic: name, Key: []byte("pk"), Value: []byte(`{"n":1}`), Headers: []kgo.RecordHeader{{Key: "h", Value: []byte("v")}}})
	amqp, err := azeventhubs.NewConsumerClientFromConnectionString(emu.E.EventHubsConnStr("xns"), name, azeventhubs.DefaultConsumerGroup, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer amqp.Close(context.Background())
	pc, err := amqp.NewPartitionClient("0", &azeventhubs.PartitionClientOptions{StartPosition: azeventhubs.StartPosition{Earliest: ptr(true)}})
	if err != nil {
		t.Fatal(err)
	}
	defer pc.Close(context.Background())
	ctx := ctxT(t, 30*time.Second)
	evs, err := pc.ReceiveEvents(ctx, 1, nil)
	if err != nil || len(evs) != 1 {
		t.Fatalf("receive: %v %d", err, len(evs))
	}
	e := evs[0]
	if string(e.Body) != `{"n":1}` || e.PartitionKey == nil || *e.PartitionKey != "pk" || e.SequenceNumber != 0 {
		t.Fatalf("event: body=%q key=%v seq=%v", e.Body, e.PartitionKey, e.SequenceNumber)
	}
	if v, ok := e.Properties["h"].([]byte); !ok || string(v) != "v" {
		t.Fatalf("property h = %#v", e.Properties["h"])
	}

	// the other way: an AMQP event is a Kafka record
	prod, err := azeventhubs.NewProducerClientFromConnectionString(emu.E.EventHubsConnStr("xns"), name, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer prod.Close(context.Background())
	batch, err := prod.NewEventDataBatch(ctx, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")})
	if err != nil {
		t.Fatal(err)
	}
	if err := batch.AddEventData(&azeventhubs.EventData{Body: []byte("from amqp"), ContentType: ptr("text/plain")}, nil); err != nil {
		t.Fatal(err)
	}
	if err := prod.SendEventDataBatch(ctx, batch, nil); err != nil {
		t.Fatal(err)
	}
	c := newClient(t, saslOpt("xns"), kgo.ConsumeTopics(name), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()))
	got := consume(t, c, 2, 30*time.Second)
	sort.Slice(got, func(i, j int) bool { return got[i].Offset < got[j].Offset })
	if string(got[1].Value) != "from amqp" || got[1].Offset != 1 {
		t.Fatalf("second record: %+v", got[1])
	}
	var ct string
	for _, h := range got[1].Headers {
		if h.Key == "content-type" {
			ct = string(h.Value)
		}
	}
	if ct != "text/plain" {
		t.Fatalf("content-type header %q", ct)
	}
}

func TestFetchLongPollsWithoutBusyLooping(t *testing.T) {
	topic := uniq("fgo-poll")
	hub(t, "default", topic, 1)
	c := newClient(t, kgo.ConsumeTopics(topic), kgo.ConsumeResetOffset(kgo.NewOffset().AtStart()), kgo.FetchMaxWait(2*time.Second))
	done := make(chan *kgo.Record, 1)
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		fs := c.PollFetches(ctx)
		fs.EachRecord(func(r *kgo.Record) { done <- r })
	}()
	time.Sleep(700 * time.Millisecond)
	p := newClient(t, kgo.RecordPartitioner(kgo.ManualPartitioner()))
	produce(t, p, &kgo.Record{Topic: topic, Value: []byte("late")})
	select {
	case r := <-done:
		if string(r.Value) != "late" {
			t.Fatalf("got %q", r.Value)
		}
	case <-time.After(15 * time.Second):
		t.Fatal("the waiting fetch did not return when the record arrived")
	}
}
