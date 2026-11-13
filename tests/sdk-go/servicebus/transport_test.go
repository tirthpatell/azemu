package servicebus

import (
	"net/http"

	"azemu-sdk-go/internal/emu"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
)

func coreOpts() azcore.ClientOptions {
	return azcore.ClientOptions{Transport: &http.Client{Transport: emu.Dump(emu.E.NewTransport())}}
}
