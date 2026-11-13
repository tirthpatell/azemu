package servicebus

import (
	"context"
	"errors"
	"fmt"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus/admin"
)

func TestMain(m *testing.M) { emu.Main(m) }

// newNS returns a unique namespace name (each test gets its own, so tests can run side by side).
func newNS() string { return emu.Name("sb") }

// nsWithQueue returns a new namespace in which the queues exist, made through the administration client before anything
// is sent: the emulator does not create queues by being named (AZEMU_SERVICEBUS_AUTOCREATE is off by default), as Azure does not.
func nsWithQueue(t testing.TB, queues ...string) string {
	t.Helper()
	ns := newNS()
	a := newAdmin(t, ns)
	for _, q := range queues {
		if _, err := a.CreateQueue(context.Background(), q, nil); err != nil {
			t.Fatalf("create queue %s: %v", q, err)
		}
	}
	return ns
}

func newClient(t testing.TB, ns string) *azservicebus.Client {
	t.Helper()
	c, err := azservicebus.NewClientFromConnectionString(emu.E.ServiceBusConnStr(ns), nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		c.Close(ctx)
	})
	return c
}

// newAdmin builds the management client. The admin client talks HTTPS to the namespace host.
func newAdmin(t testing.TB, ns string) *admin.Client {
	t.Helper()
	a, err := admin.NewClientFromConnectionString(emu.E.ServiceBusConnStr(ns), &admin.ClientOptions{ClientOptions: coreOpts()})
	if err != nil {
		t.Fatal(err)
	}
	return a
}

func ctxT(t testing.TB, d time.Duration) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), d)
	t.Cleanup(cancel)
	return ctx
}

func send(t testing.TB, c *azservicebus.Client, queue string, msgs ...*azservicebus.Message) {
	t.Helper()
	s, err := c.NewSender(queue, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close(context.Background())
	for _, m := range msgs {
		if err := s.SendMessage(ctxT(t, 20*time.Second), m, nil); err != nil {
			t.Fatalf("send: %v", err)
		}
	}
}

func text(s string) *azservicebus.Message { return &azservicebus.Message{Body: []byte(s)} }

// recvOne receives exactly one message (peek-lock) or fails.
func recvOne(t testing.TB, r *azservicebus.Receiver) *azservicebus.ReceivedMessage {
	t.Helper()
	ms, err := r.ReceiveMessages(ctxT(t, 20*time.Second), 1, nil)
	if err != nil {
		t.Fatalf("receive: %v", err)
	}
	if len(ms) != 1 {
		t.Fatalf("received %d messages, want 1", len(ms))
	}
	return ms[0]
}

func newReceiver(t testing.TB, c *azservicebus.Client, queue string, o *azservicebus.ReceiverOptions) *azservicebus.Receiver {
	t.Helper()
	r, err := c.NewReceiverForQueue(queue, o)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { r.Close(context.Background()) })
	return r
}

func asSBError(err error) *azservicebus.Error {
	var e *azservicebus.Error
	if errors.As(err, &e) {
		return e
	}
	return nil
}

func ptr[T any](v T) *T { return &v }

var _ = fmt.Sprint

func asRE(err error, target **azcore.ResponseError) bool { return errors.As(err, target) }
