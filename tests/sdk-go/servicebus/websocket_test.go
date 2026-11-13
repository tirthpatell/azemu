package servicebus

import (
	"context"
	"net"
	"net/http"
	"strings"
	"sync"
	"testing"
	"time"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/coder/websocket"
)

// wsClient builds a client that carries AMQP inside a WebSocket (the SDK calls NewWebSocketConn instead of dialling TCP).
// Plain "ws" or TLS "wss" depends on the scheme the test picks.
func wsClient(t testing.TB, ns, scheme string) (*azservicebus.Client, *wsLog) {
	t.Helper()
	log := &wsLog{}
	opts := &azservicebus.ClientOptions{
		NewWebSocketConn: func(ctx context.Context, args azservicebus.NewWebSocketConnArgs) (net.Conn, error) {
			u := strings.Replace(args.Host, "wss://", scheme+"://", 1)
			log.add(u)
			// websocket needs HTTP/1.1; the emulator's CA is trusted by this client only
			hc := &http.Client{Transport: emu.E.HTTP1Transport()}
			conn, _, err := websocket.Dial(ctx, u, &websocket.DialOptions{HTTPClient: hc, Subprotocols: []string{"amqp"}})
			if err != nil {
				return nil, err
			}
			return websocket.NetConn(ctx, conn, websocket.MessageBinary), nil
		},
	}
	c, err := azservicebus.NewClientFromConnectionString(emu.E.ServiceBusConnStr(ns), opts)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close(context.Background()) })
	return c, log
}

type wsLog struct {
	mu   sync.Mutex
	urls []string
}

func (l *wsLog) add(u string) { l.mu.Lock(); l.urls = append(l.urls, u); l.mu.Unlock() }
func (l *wsLog) n() int       { l.mu.Lock(); defer l.mu.Unlock(); return len(l.urls) }

func TestWebSocket_SendAndReceiveOverWssWithTheCaTrustedByTheClient(t *testing.T) {
	c, log := wsClient(t, nsWithQueue(t, "ws-q"), "wss")
	send(t, c, "ws-q", &azservicebus.Message{Body: []byte("over websocket"), Subject: ptr("s")})
	r := newReceiver(t, c, "ws-q", nil)
	m := recvOne(t, r)
	if string(m.Body) != "over websocket" || *m.Subject != "s" {
		t.Fatalf("%+v", m)
	}
	r.CompleteMessage(context.Background(), m, nil)
	if log.n() == 0 {
		t.Fatal("the SDK never called NewWebSocketConn")
	}
}

func TestWebSocket_PlainWsAlsoWorksAndSharesStateWithATcpClient(t *testing.T) {
	ns := nsWithQueue(t, "ws-share")
	ws, _ := wsClient(t, ns, "ws")
	tcp := newClient(t, ns)
	send(t, ws, "ws-share", text("via ws"))
	r := newReceiver(t, tcp, "ws-share", nil)
	m := recvOne(t, r)
	if string(m.Body) != "via ws" {
		t.Fatalf("%q", m.Body)
	}
	r.CompleteMessage(context.Background(), m, nil)
}

func TestWebSocket_ABatchOfLargeMessagesSpansManyWebSocketFrames(t *testing.T) {
	c, _ := wsClient(t, nsWithQueue(t, "ws-big"), "wss")
	body := make([]byte, 240*1024)
	for i := range body {
		body[i] = byte(i)
	}
	send(t, c, "ws-big", &azservicebus.Message{Body: body})
	r := newReceiver(t, c, "ws-big", &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
	m := recvOne(t, r)
	if len(m.Body) != len(body) || m.Body[100000] != body[100000] {
		t.Fatalf("size %d", len(m.Body))
	}
}

func TestWebSocket_ConnectionStaysUsableAcrossAnIdlePeriodOfSeveralSeconds(t *testing.T) {
	c, _ := wsClient(t, nsWithQueue(t, "ws-idle"), "wss")
	send(t, c, "ws-idle", text("1"))
	time.Sleep(4 * time.Second)
	send(t, c, "ws-idle", text("2"))
	r := newReceiver(t, c, "ws-idle", &azservicebus.ReceiverOptions{ReceiveMode: azservicebus.ReceiveModeReceiveAndDelete})
	ms, err := r.ReceiveMessages(ctxT(t, 10*time.Second), 2, &azservicebus.ReceiveMessagesOptions{TimeAfterFirstMessage: time.Second})
	if err != nil || len(ms) != 2 {
		t.Fatalf("%v %d", err, len(ms))
	}
}

func TestTls_AmqpOverTlsWithATokenCredentialAndACustomEndpointPort(t *testing.T) {
	ns := nsWithQueue(t, "tls-q")
	host := ns + ".servicebus.localhost"
	c, err := azservicebus.NewClient(host, emu.Cred(), &azservicebus.ClientOptions{
		TLSConfig:      emu.E.TLSConfig(),
		CustomEndpoint: emu.E.Host("servicebus", ns),
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close(context.Background()) })
	send(t, c, "tls-q", text("over tls"))
	r := newReceiver(t, c, "tls-q", nil)
	m := recvOne(t, r)
	if string(m.Body) != "over tls" {
		t.Fatalf("%q", m.Body)
	}
	r.CompleteMessage(context.Background(), m, nil)
}
