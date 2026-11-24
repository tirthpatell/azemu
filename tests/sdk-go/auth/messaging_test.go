package auth

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus/admin"
)

// A namespace the emulator was not asked to make through Resource Manager has the rule RootManageSharedAccessKey with the development key
// SAS_KEY_VALUE (the one Microsoft's own emulator shows); more rules are declared through the admin API of the service.

const devKey = "SAS_KEY_VALUE"

func sbConn(ns, rule, key string) string {
	return fmt.Sprintf("Endpoint=sb://%s.servicebus.localhost:%d;SharedAccessKeyName=%s;SharedAccessKey=%s;UseDevelopmentEmulator=true", ns, emu.E.Port, rule, key)
}

func declareRule(t *testing.T, ns, name, rights, key string) {
	t.Helper()
	body := fmt.Sprintf(`{"rights":%s,"primaryKey":%q}`, rights, key)
	req, _ := http.NewRequest("PUT", fmt.Sprintf("%s/_azemu/servicebus/%s/authorization-rules/%s", emu.E.Gateway(), ns, name), bytes.NewReader([]byte(body)))
	req.Header.Set("Content-Type", "application/json")
	r, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Body.Close()
	if r.StatusCode != 201 && r.StatusCode != 200 {
		b, _ := io.ReadAll(r.Body)
		t.Fatalf("declare %s: %d %s", name, r.StatusCode, b)
	}
}

func TestServiceBus_ASendWithAListenOnlyKeyIsRefusedAndWithASendKeyAccepted(t *testing.T) {
	ns := emu.Name("gosb")
	declareRule(t, ns, "listener", `["Listen"]`, "listen-key")
	declareRule(t, ns, "sender", `["Send"]`, "send-key")

	// the queue is made with the root rule (Manage)
	a, err := admin.NewClientFromConnectionString(sbConn(ns, "RootManageSharedAccessKey", devKey), &admin.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: &http.Client{Transport: emu.E.NewTransport()}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := a.CreateQueue(context.Background(), "orders", nil); err != nil {
		t.Fatalf("create queue with the root rule: %v", err)
	}

	client := func(rule, key string) *azservicebus.Client {
		c, err := azservicebus.NewClientFromConnectionString(sbConn(ns, rule, key), nil)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { c.Close(context.Background()) })
		return c
	}
	send := func(c *azservicebus.Client) error {
		s, err := c.NewSender("orders", nil)
		if err != nil {
			return err
		}
		defer s.Close(context.Background())
		ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		defer cancel()
		return s.SendMessage(ctx, &azservicebus.Message{Body: []byte("hello")}, nil)
	}

	// a key that only listens: the error a Go program sees is an *azservicebus.Error with the unauthorized-access code
	err = send(client("listener", "listen-key"))
	var sbErr *azservicebus.Error
	if !errors.As(err, &sbErr) {
		t.Fatalf("not an *azservicebus.Error: %T %v", err, err)
	}
	if sbErr.Code != azservicebus.CodeUnauthorizedAccess || !strings.Contains(err.Error(), "'Send' claim(s) are required") {
		t.Fatalf("code %q: %v", sbErr.Code, err)
	}

	// a key that sends, and the root rule with the development key, send; a key that is not one is refused at the login
	if err := send(client("sender", "send-key")); err != nil {
		t.Fatalf("send with a Send key: %v", err)
	}
	r, err := client("listener", "listen-key").NewReceiverForQueue("orders", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close(context.Background())
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	msgs, err := r.ReceiveMessages(ctx, 1, nil)
	if err != nil || len(msgs) != 1 || string(msgs[0].Body) != "hello" {
		t.Fatalf("receive with a Listen key: %v %d", err, len(msgs))
	}
	_ = r.CompleteMessage(context.Background(), msgs[0], nil)
	if err := send(client("RootManageSharedAccessKey", devKey)); err != nil {
		t.Fatalf("send with the root rule: %v", err)
	}
	if err := send(client("RootManageSharedAccessKey", "not-the-key")); err == nil {
		t.Fatal("a wrong key was accepted")
	}
}
