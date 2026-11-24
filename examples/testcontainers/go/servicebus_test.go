package example

import (
	"context"
	"testing"
	"time"

	"azemu-testcontainers-go/emulator"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus"
	"github.com/Azure/azure-sdk-for-go/sdk/messaging/azservicebus/admin"
)

// AMQP on the published port, in plain text (UseDevelopmentEmulator=true). The administration client always speaks
// https to the same host and port, so it gets the CA.
func TestServiceBusSendAndReceive(t *testing.T) {
	e := startEmulator(t)
	conn := "Endpoint=sb://" + e.Host + ":" + e.Port(emulator.Bus) + ";SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true"
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// Nothing exists until it is created, as on Azure.
	adm, err := admin.NewClientFromConnectionString(conn, &admin.ClientOptions{ClientOptions: azcore.ClientOptions{Transport: trusting(t, e)}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := adm.CreateQueue(ctx, "commands", nil); err != nil {
		t.Fatal(err)
	}

	client, err := azservicebus.NewClientFromConnectionString(conn, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close(ctx)
	sender, err := client.NewSender("commands", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer sender.Close(ctx)
	if err := sender.SendMessage(ctx, &azservicebus.Message{Body: []byte("ship order 1")}, nil); err != nil {
		t.Fatal(err)
	}
	receiver, err := client.NewReceiverForQueue("commands", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer receiver.Close(ctx)
	messages, err := receiver.ReceiveMessages(ctx, 1, nil)
	if err != nil || len(messages) != 1 || string(messages[0].Body) != "ship order 1" {
		t.Fatalf("received %v, %v", messages, err)
	}
	if err := receiver.CompleteMessage(ctx, messages[0], nil); err != nil {
		t.Fatal(err)
	}
}
