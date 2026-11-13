package eventhubs

import (
	"context"
	"errors"
	"net"
	"net/http"
	"strings"
	"sync"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azeventhubs/v2"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azeventhubs/v2/checkpoints"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
	"github.com/coder/websocket"
)

// checkpointContainer makes a blob container on the development storage account of the same emulator.
func checkpointContainer(t testing.TB) *container.Client {
	t.Helper()
	cred, err := azblob.NewSharedKeyCredential(emu.DevAccount, emu.DevKey)
	if err != nil {
		t.Fatal(err)
	}
	name := emu.Name("ckpt")
	c, err := container.NewClientWithSharedKeyCredential(emu.E.HTTP("blob", emu.DevAccount)+"/"+name, cred, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.Create(context.Background(), nil); err != nil {
		t.Fatal(err)
	}
	return c
}

func TestCheckpointStore_ClaimOwnershipThenListOwnershipAndSecondClaimWithAStaleETagFails(t *testing.T) {
	ctx := context.Background()
	store, err := checkpoints.NewBlobStore(checkpointContainer(t), nil)
	if err != nil {
		t.Fatal(err)
	}
	const fq, hub, group = "ns.eventhub.localhost", "hub", "$Default"
	own := azeventhubs.Ownership{ConsumerGroup: group, EventHubName: hub, FullyQualifiedNamespace: fq, PartitionID: "0", OwnerID: "proc-1"}
	got, err := store.ClaimOwnership(ctx, []azeventhubs.Ownership{own}, nil)
	if err != nil || len(got) != 1 || got[0].ETag == nil {
		t.Fatalf("claim: %v %+v", err, got)
	}
	listed, err := store.ListOwnership(ctx, fq, hub, group, nil)
	if err != nil || len(listed) != 1 || listed[0].OwnerID != "proc-1" || listed[0].PartitionID != "0" {
		t.Fatalf("list ownership: %v %+v", err, listed)
	}
	// proc-2 claims with the current etag: allowed (steals). proc-3 claims with the old etag: refused
	stale := got[0]
	stale.OwnerID = "proc-2"
	if won, err := store.ClaimOwnership(ctx, []azeventhubs.Ownership{stale}, nil); err != nil || len(won) != 1 {
		t.Fatalf("second claim: %v %d", err, len(won))
	}
	stale.OwnerID = "proc-3"
	won, err := store.ClaimOwnership(ctx, []azeventhubs.Ownership{stale}, nil)
	if err != nil || len(won) != 0 {
		t.Fatalf("claim with a stale etag: %v won %d", err, len(won))
	}
}

func TestCheckpointStore_SetAndListCheckpointsKeepOffsetAndSequenceNumber(t *testing.T) {
	ctx := context.Background()
	store, _ := checkpoints.NewBlobStore(checkpointContainer(t), nil)
	const fq, hub, group = "ns.eventhub.localhost", "hub", "$Default"
	for i, pid := range []string{"0", "1"} {
		off, seq := "4096", int64(10+i)
		err := store.SetCheckpoint(ctx, azeventhubs.Checkpoint{ConsumerGroup: group, EventHubName: hub, FullyQualifiedNamespace: fq, PartitionID: pid, Offset: &off, SequenceNumber: &seq}, nil)
		if err != nil {
			t.Fatalf("set checkpoint: %v", err)
		}
	}
	cps, err := store.ListCheckpoints(ctx, fq, hub, group, nil)
	if err != nil || len(cps) != 2 {
		t.Fatalf("list: %v %d", err, len(cps))
	}
	for _, cp := range cps {
		if *cp.Offset != "4096" || cp.SequenceNumber == nil || (*cp.SequenceNumber != 10 && *cp.SequenceNumber != 11) {
			t.Fatalf("checkpoint %+v", cp)
		}
	}
	// a different consumer group has none
	other, _ := store.ListCheckpoints(ctx, fq, hub, "other", nil)
	if len(other) != 0 {
		t.Fatalf("other group has %d", len(other))
	}
}

// runProcessor processes partitions until ctx ends: each partition client reads events, records them and checkpoints.
func runProcessor(ctx context.Context, t testing.TB, proc *azeventhubs.Processor, onEvent func(partition string, e *azeventhubs.ReceivedEventData)) {
	var wg sync.WaitGroup
	go func() {
		for {
			pc := proc.NextPartitionClient(ctx)
			if pc == nil {
				return
			}
			wg.Add(1)
			go func() {
				defer wg.Done()
				defer pc.Close(context.Background())
				for ctx.Err() == nil {
					// ReceiveEvents waits for the full count unless its context ends, so give each call a short one and keep what it returned
					callCtx, cancelCall := context.WithTimeout(ctx, time.Second)
					evs, err := pc.ReceiveEvents(callCtx, 50, nil)
					cancelCall()
					for _, e := range evs {
						onEvent(pc.PartitionID(), e)
					}
					if len(evs) > 0 {
						// the checkpoint is written with its own context, so ending the processor cannot cancel it
						ucx, cancelU := context.WithTimeout(context.Background(), 10*time.Second)
						if uerr := pc.UpdateCheckpoint(ucx, evs[len(evs)-1], nil); uerr != nil {
							t.Errorf("update checkpoint: %v", uerr)
						}
						cancelU()
					}
					if err != nil && !errors.Is(err, context.DeadlineExceeded) && ctx.Err() == nil {
						t.Errorf("receive events: %v", err)
						return
					}
				}
			}()
		}
	}()
	proc.Run(ctx)
	wg.Wait()
}

func TestProcessor_OwnsAllFourPartitionsReadsEverythingAndCheckpointsSoARestartSkipsProcessedEvents(t *testing.T) {
	ns := newNS()
	skipIfNoGeoReplicationFactor(t, ns, "proc1")
	p := newProducer(t, ns, "proc1")
	store, _ := checkpoints.NewBlobStore(checkpointContainer(t), nil)
	for _, pid := range []string{"0", "1", "2", "3"} {
		sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr(pid)}, ev("first-"+pid))
	}
	consumer := newConsumer(t, ns, "proc1", "")
	start := func(d time.Duration) map[string]int {
		proc, err := azeventhubs.NewProcessor(consumer, store, &azeventhubs.ProcessorOptions{
			UpdateInterval: time.Second, StartPositions: azeventhubs.StartPositions{Default: earliest()}})
		if err != nil {
			t.Fatal(err)
		}
		var mu sync.Mutex
		seen := map[string]int{}
		ctx, cancel := context.WithTimeout(context.Background(), d)
		defer cancel()
		runProcessor(ctx, t, proc, func(pid string, e *azeventhubs.ReceivedEventData) {
			mu.Lock()
			seen[pid]++
			t.Logf("partition %s event %q seq %d at %s", pid, e.Body, e.SequenceNumber, time.Now().Format("15:04:05.000"))
			mu.Unlock()
		})
		return seen
	}
	seen := start(10 * time.Second)
	if len(seen) != 4 {
		t.Fatalf("first run saw partitions %v", seen)
	}
	for _, pid := range []string{"0", "1", "2", "3"} {
		sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr(pid)}, ev("second-"+pid))
	}
	seen = start(10 * time.Second)
	for pid, n := range seen {
		if n != 1 {
			t.Fatalf("after the restart partition %s delivered %d events, want only the new one", pid, n)
		}
	}
	if len(seen) != 4 {
		t.Fatalf("second run saw %v", seen)
	}
}

func TestProcessor_TwoProcessorsSharingOneCheckpointStoreSplitThePartitionsBetweenThem(t *testing.T) {
	ns := newNS()
	skipIfNoGeoReplicationFactor(t, ns, "proc2")
	p := newProducer(t, ns, "proc2")
	for _, pid := range []string{"0", "1", "2", "3"} {
		sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr(pid)}, ev("x"))
	}
	store, _ := checkpoints.NewBlobStore(checkpointContainer(t), nil)
	var mu sync.Mutex
	owned := [2]map[string]bool{{}, {}}
	var wg sync.WaitGroup
	ctx, cancel := context.WithTimeout(context.Background(), 40*time.Second)
	defer cancel()
	for i := 0; i < 2; i++ {
		i := i
		consumer := newConsumer(t, ns, "proc2", "")
		proc, err := azeventhubs.NewProcessor(consumer, store, &azeventhubs.ProcessorOptions{
			UpdateInterval: 2 * time.Second, PartitionExpirationDuration: 10 * time.Second, StartPositions: azeventhubs.StartPositions{Default: earliest()}})
		if err != nil {
			t.Fatal(err)
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			runProcessor(ctx, t, proc, func(pid string, e *azeventhubs.ReceivedEventData) { mu.Lock(); owned[i][pid] = true; mu.Unlock() })
		}()
	}
	time.Sleep(20 * time.Second)
	cancel()
	wg.Wait()
	mu.Lock()
	defer mu.Unlock()
	if len(owned[0])+len(owned[1]) < 4 || len(owned[0]) == 0 || len(owned[1]) == 0 {
		t.Fatalf("processor 1 read %v, processor 2 read %v: expected both to own some partitions", owned[0], owned[1])
	}
}

// ---- other transports ----

func TestWebSocket_ProducerAndConsumerOverWss(t *testing.T) {
	ns := newNS()
	dial := func(ctx context.Context, params azeventhubs.WebSocketConnParams) (net.Conn, error) {
		hc := &http.Client{Transport: emu.E.HTTP1Transport()}
		conn, _, err := websocket.Dial(ctx, params.Host, &websocket.DialOptions{HTTPClient: hc, Subprotocols: []string{"amqp"}})
		if err != nil {
			return nil, err
		}
		return websocket.NetConn(ctx, conn, websocket.MessageBinary), nil
	}
	ensureHub(t, ns, "wshub", "")
	p, err := azeventhubs.NewProducerClientFromConnectionString(connStr(ns), "wshub", &azeventhubs.ProducerClientOptions{NewWebSocketConn: dial})
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close(context.Background())
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("ws event"))
	c, err := azeventhubs.NewConsumerClientFromConnectionString(connStr(ns), "wshub", azeventhubs.DefaultConsumerGroup, &azeventhubs.ConsumerClientOptions{NewWebSocketConn: dial})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close(context.Background())
	got := receive(t, c, "0", earliest(), 1, 20*time.Second)
	if len(got) != 1 || string(got[0].Body) != "ws event" {
		t.Fatalf("received %d events", len(got))
	}
}

func TestTls_ProducerAndConsumerWithATokenCredentialAndTheEmulatorsCa(t *testing.T) {
	ns := newNS()
	host := ns + ".eventhub.localhost"
	custom := emu.E.Host("eventhub", ns)
	ensureHub(t, ns, "tlshub", "")
	p, err := azeventhubs.NewProducerClient(host, "tlshub", emu.Cred(), &azeventhubs.ProducerClientOptions{TLSConfig: emu.E.TLSConfig(), CustomEndpoint: custom})
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close(context.Background())
	sendTo(t, p, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("tls event"))
	c, err := azeventhubs.NewConsumerClient(host, "tlshub", azeventhubs.DefaultConsumerGroup, emu.Cred(), &azeventhubs.ConsumerClientOptions{TLSConfig: emu.E.TLSConfig(), CustomEndpoint: custom})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close(context.Background())
	got := receive(t, c, "0", earliest(), 1, 20*time.Second)
	if len(got) != 1 || string(got[0].Body) != "tls event" {
		t.Fatalf("received %d events", len(got))
	}
}

func TestSend_ToAHubThatDoesNotExistCreatesItOnFirstUseLikeTheEmulatorSaysAndAnotherNamespaceIsSeparate(t *testing.T) {
	p1 := newProducer(t, newNS(), "same-name")
	p2 := newProducer(t, newNS(), "same-name")
	sendTo(t, p1, &azeventhubs.EventDataBatchOptions{PartitionID: ptr("0")}, ev("ns1"))
	pp, err := p2.GetPartitionProperties(ctxT(t, 10*time.Second), "0", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !pp.IsEmpty {
		t.Fatal("a hub of the same name in another namespace must be a separate hub")
	}
}

var _ = errors.New
var _ = azcore.ETag("")
var _ = strings.TrimSpace
