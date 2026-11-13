package eventhubs

import (
	"context"
	"errors"
	"io"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azeventhubs/v2"
)

func TestMain(m *testing.M) { emu.Main(m) }

func newNS() string { return emu.Name("eh") }

func connStr(ns string) string { return emu.E.EventHubsConnStr(ns) }

// ensureHub creates the event hub (and the consumer group, when one is named) through the emulator's admin API before the
// first client uses it: the emulator does not create hubs by being named (AZEMU_EVENTHUBS_AUTOCREATE is off by default), as
// Azure does not. A hub that exists is left as it is; only the group is added.
func ensureHub(t testing.TB, ns, hub, group string) {
	t.Helper()
	body := `{}`
	if group != "" && group != azeventhubs.DefaultConsumerGroup {
		body = `{"consumerGroups":["` + group + `"]}`
	}
	req, err := http.NewRequest(http.MethodPut, emu.E.Gateway()+"/_azemu/eventhubs/"+ns+"/hubs/"+hub, strings.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 && resp.StatusCode != 201 {
		b, _ := io.ReadAll(resp.Body)
		t.Fatalf("create event hub %s/%s: %d %s", ns, hub, resp.StatusCode, b)
	}
}

func newProducer(t testing.TB, ns, hub string) *azeventhubs.ProducerClient {
	t.Helper()
	ensureHub(t, ns, hub, "")
	p, err := azeventhubs.NewProducerClientFromConnectionString(connStr(ns), hub, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { p.Close(context.Background()) })
	return p
}

func newConsumer(t testing.TB, ns, hub, group string) *azeventhubs.ConsumerClient {
	t.Helper()
	if group == "" {
		group = azeventhubs.DefaultConsumerGroup
	}
	ensureHub(t, ns, hub, group)
	c, err := azeventhubs.NewConsumerClientFromConnectionString(connStr(ns), hub, group, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close(context.Background()) })
	return c
}

func ctxT(t testing.TB, d time.Duration) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), d)
	t.Cleanup(cancel)
	return ctx
}

// sendTo sends the bodies as one batch to a partition (or by key when partitionID is empty and key is set).
func sendTo(t testing.TB, p *azeventhubs.ProducerClient, opts *azeventhubs.EventDataBatchOptions, events ...*azeventhubs.EventData) {
	t.Helper()
	ctx := ctxT(t, 30*time.Second)
	b, err := p.NewEventDataBatch(ctx, opts)
	if err != nil {
		t.Fatalf("new batch: %v", err)
	}
	for _, e := range events {
		if err := b.AddEventData(e, nil); err != nil {
			t.Fatalf("add: %v", err)
		}
	}
	if err := p.SendEventDataBatch(ctx, b, nil); err != nil {
		t.Fatalf("send: %v", err)
	}
}

func ev(body string) *azeventhubs.EventData { return &azeventhubs.EventData{Body: []byte(body)} }

func ptr[T any](v T) *T { return &v }

func earliest() azeventhubs.StartPosition { return azeventhubs.StartPosition{Earliest: ptr(true)} }

// receive reads up to n events from a partition from the position, waiting at most d.
func receive(t testing.TB, c *azeventhubs.ConsumerClient, partition string, pos azeventhubs.StartPosition, n int, d time.Duration) []*azeventhubs.ReceivedEventData {
	t.Helper()
	pc, err := c.NewPartitionClient(partition, &azeventhubs.PartitionClientOptions{StartPosition: pos})
	if err != nil {
		t.Fatal(err)
	}
	defer pc.Close(context.Background())
	ctx, cancel := context.WithTimeout(context.Background(), d)
	defer cancel()
	var out []*azeventhubs.ReceivedEventData
	for len(out) < n {
		evs, err := pc.ReceiveEvents(ctx, n-len(out), nil)
		out = append(out, evs...)
		if err != nil {
			break
		}
	}
	return out
}

func asEH(err error, target **azeventhubs.Error) bool { return errors.As(err, target) }

// skipIfNoGeoReplicationFactor skips the test (a known difference) while the emulator's answer to the event hub READ lacks
// georeplication_factor, which azeventhubs v2 requires; it runs the test as soon as the answer is complete.
func skipIfNoGeoReplicationFactor(t *testing.T, ns, hub string) {
	t.Helper()
	ensureHub(t, ns, hub, "")
	p, err := azeventhubs.NewProducerClientFromConnectionString(connStr(ns), hub, &azeventhubs.ProducerClientOptions{RetryOptions: azeventhubs.RetryOptions{MaxRetries: -1}})
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close(context.Background())
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err := p.GetEventHubProperties(ctx, nil); err != nil {
		if strings.Contains(err.Error(), "georeplication_factor") {
			emu.Finding(t, "azemu: the READ answer for com.microsoft:eventhub has no georeplication_factor, which azeventhubs v2 requires")
		}
		t.Fatalf("event hub properties: %v", err)
	}
}
