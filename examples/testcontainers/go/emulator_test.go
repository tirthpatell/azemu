package example

import (
	"context"
	"net/http"
	"testing"

	"azemu-testcontainers-go/emulator"
)

// startEmulator gives the test a container of its own, removed when the test ends (about 0.3 s).
func startEmulator(t *testing.T) *emulator.Emulator {
	t.Helper()
	e, err := emulator.Start(context.Background(), "seed.json")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(e.Terminate)
	return e
}

// trusting returns an HTTP client that trusts this container's CA.
func trusting(t *testing.T, e *emulator.Emulator) *http.Client {
	t.Helper()
	c, err := e.HTTPClient()
	if err != nil {
		t.Fatal(err)
	}
	return c
}
