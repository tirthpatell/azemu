// Package emulator starts azemu in a container with Testcontainers.
package emulator

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"fmt"
	"io"
	"net/http"
	"net/netip"
	"os"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/network"
	"github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/wait"
)

// Pin the version: below 1.0 a minor version may change a flag or a setting.
const Image = "ghcr.io/tirthpatell/azemu:0.3.0"

// The gateway, and the standard ports of the services these tests use.
const (
	Gateway  = "4577/tcp"
	Blob     = "10000/tcp"
	KeyVault = "10010/tcp"
	Bus      = "5672/tcp"
)

type Emulator struct {
	ctr  testcontainers.Container
	Host string
}

// Start returns when GET /_azemu/health answers 200. seedFile is copied into the container and used as AZEMU_SEED.
func Start(ctx context.Context, seedFile string) (*Emulator, error) {
	ports := []string{Gateway, Blob, KeyVault, Bus}
	env := map[string]string{"AZEMU_SEED": "/seed.json"}
	if name := os.Getenv("AZEMU_TLS_NAME"); name != "" {
		env["AZEMU_TLS_NAME"] = name // for a Docker host that is not "localhost": see the page
	}
	ctr, err := testcontainers.Run(ctx, Image,
		testcontainers.WithExposedPorts(ports...),
		testcontainers.WithWaitStrategy(wait.ForHTTP("/_azemu/health").WithPort(Gateway).WithStatusCodeMatcher(func(s int) bool { return s == 200 })),
		// A seed file: copied into the container (a bind mount would need the file on the Docker host).
		testcontainers.WithFiles(testcontainers.ContainerFile{HostFilePath: seedFile, ContainerFilePath: "/seed.json", FileMode: 0o644}),
		testcontainers.WithEnv(env),
		// The emulator has no sign-in: publish only on the loopback address (the default is every address).
		// Remove this for a Docker host on another machine.
		testcontainers.WithHostConfigModifier(func(hc *container.HostConfig) {
			hc.PortBindings = network.PortMap{}
			for _, p := range ports {
				hc.PortBindings[network.MustParsePort(p)] = []network.PortBinding{{HostIP: netip.MustParseAddr("127.0.0.1"), HostPort: "0"}}
			}
		}),
	)
	if err != nil {
		testcontainers.TerminateContainer(ctr)
		return nil, err
	}
	host, err := ctr.Host(ctx)
	if err != nil {
		testcontainers.TerminateContainer(ctr)
		return nil, err
	}
	return &Emulator{ctr: ctr, Host: host}, nil
}

// Terminate stops and removes the container.
func (e *Emulator) Terminate() { testcontainers.TerminateContainer(e.ctr) }

// Port is the host port that a container port is published on (a different one for every container).
func (e *Emulator) Port(containerPort string) string {
	p, err := e.ctr.MappedPort(context.Background(), containerPort)
	if err != nil {
		panic(err) // only when the port was not published
	}
	return p.Port()
}

// URL builds an address on a published port.
func (e *Emulator) URL(scheme, containerPort, path string) string {
	return fmt.Sprintf("%s://%s:%s%s", scheme, e.Host, e.Port(containerPort), path)
}

// HTTPClient trusts the CA of this container and no other: a new authority is made at every start, and it is fetched from the container.
func (e *Emulator) HTTPClient() (*http.Client, error) {
	res, err := http.Get(e.URL("http", Gateway, "/_azemu/ca.pem"))
	if err != nil {
		return nil, err
	}
	defer res.Body.Close()
	pem, _ := io.ReadAll(res.Body)
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(pem) {
		return nil, fmt.Errorf("ca.pem holds no certificate")
	}
	return &http.Client{Transport: &http.Transport{TLSClientConfig: &tls.Config{RootCAs: pool, MinVersion: tls.VersionTLS12}}}, nil
}

// Reset empties every service; the seed file is applied again.
func (e *Emulator) Reset() error {
	res, err := http.Post(e.URL("http", Gateway, "/_azemu/reset"), "", nil)
	if err != nil {
		return err
	}
	res.Body.Close()
	if res.StatusCode != http.StatusOK {
		return fmt.Errorf("reset: %s", res.Status)
	}
	return nil
}
