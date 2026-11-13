// Package emu starts one azemu process for a test package and hands out URLs, TLS settings and credentials.
//
// Every test package calls emu.Main from TestMain. The emulator listens on a free port, keeps its certificate
// authority in a temporary directory and is stopped when the package's tests are done.
package emu

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
)

// The well-known development account (same as Azurite).
const (
	DevAccount = "devstoreaccount1"
	DevKey     = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
)

// Emu is a running emulator.
type Emu struct {
	Port    int
	CertDir string
	CAFile  string
	Pool    *x509.CertPool
	Log     string
	Bin     string
	cmd     *exec.Cmd
	done    chan struct{}
}

// E is the emulator of the current test package (set by Main).
var E *Emu

var counter atomic.Uint64

// ModuleRoot returns tests/sdk-go.
func ModuleRoot() string {
	_, file, _, _ := runtime.Caller(0)
	return filepath.Clean(filepath.Join(filepath.Dir(file), "..", ".."))
}

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

// Main is the body of TestMain: it starts the emulator, runs the tests and stops the emulator.
func Main(m *testing.M, extraArgs ...string) {
	code := 1
	func() {
		e, err := Start(extraArgs...)
		if err != nil {
			fmt.Fprintln(os.Stderr, "cannot start azemu:", err)
			return
		}
		E = e
		defer e.Stop()
		code = m.Run()
	}()
	os.Exit(code)
}

// Start launches the emulator.
func Start(extraArgs ...string) (*Emu, error) { return StartEnv(nil, extraArgs...) }

// StartEnv launches the emulator with extra environment entries ("NAME=value"), for the few tests that need an emulator
// with a setting of its own (for example AZEMU_EVENTHUBS_AUTOCREATE=1). Stop it with Stop.
func StartEnv(env []string, extraArgs ...string) (*Emu, error) {
	bin := os.Getenv("AZEMU_BIN")
	if bin == "" {
		bin = filepath.Join(ModuleRoot(), ".azemu-under-test")
	}
	if _, err := os.Stat(bin); err != nil {
		return nil, fmt.Errorf("emulator binary %s: %w (copy target/debug/azemu there, or set AZEMU_BIN)", bin, err)
	}
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	dir, err := os.MkdirTemp("", "azemu-go-")
	if err != nil {
		return nil, err
	}
	logPath := filepath.Join(dir, "azemu.log")
	logf, err := os.Create(logPath)
	if err != nil {
		return nil, err
	}
	args := []string{"--port", fmt.Sprint(port), "--cert-dir", dir, "--log-level", "debug"}
	args = append(args, extraArgs...)
	cmd := exec.Command(bin, args...)
	if len(env) > 0 {
		cmd.Env = append(os.Environ(), env...)
	}
	cmd.Stdout = logf
	cmd.Stderr = logf
	// the emulator must not outlive a test binary that crashed or was killed (Linux)
	cmd.SysProcAttr = &syscall.SysProcAttr{Pdeathsig: syscall.SIGKILL}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	e := &Emu{Port: port, CertDir: dir, CAFile: filepath.Join(dir, "ca.pem"), Log: logPath, Bin: bin, cmd: cmd, done: make(chan struct{})}
	go func() { _ = cmd.Wait(); close(e.done) }()
	deadline := time.Now().Add(30 * time.Second)
	for {
		select {
		case <-e.done:
			return nil, fmt.Errorf("emulator exited early; log %s", logPath)
		default:
		}
		resp, err := http.Get(fmt.Sprintf("http://127.0.0.1:%d/_azemu/health", port))
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode == 200 {
				break
			}
		}
		if time.Now().After(deadline) {
			e.Stop()
			return nil, fmt.Errorf("emulator not healthy on port %d; log %s", port, logPath)
		}
		time.Sleep(100 * time.Millisecond)
	}
	pem, err := os.ReadFile(e.CAFile)
	if err != nil {
		e.Stop()
		return nil, err
	}
	e.Pool = x509.NewCertPool()
	if !e.Pool.AppendCertsFromPEM(pem) {
		e.Stop()
		return nil, fmt.Errorf("ca.pem has no certificate")
	}
	return e, nil
}

// Stop ends the process and removes the temporary directory (the log is copied to AZEMU_KEEP_LOG when set).
func (e *Emu) Stop() {
	if e.cmd != nil && e.cmd.Process != nil {
		_ = e.cmd.Process.Signal(syscall.SIGTERM)
		select {
		case <-e.done:
		case <-time.After(5 * time.Second):
			_ = e.cmd.Process.Kill()
			<-e.done
		}
	}
	if keep := os.Getenv("AZEMU_KEEP_LOG"); keep != "" {
		if b, err := os.ReadFile(e.Log); err == nil {
			_ = os.WriteFile(keep, b, 0o644)
		}
	}
	_ = os.RemoveAll(e.CertDir)
}

// Reset drops all emulator state.
func (e *Emu) Reset() error {
	req, _ := http.NewRequest(http.MethodPost, e.Gateway()+"/_azemu/reset", strings.NewReader(""))
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return err
	}
	io.Copy(io.Discard, resp.Body)
	return resp.Body.Close()
}

// Gateway is the plain HTTP address of the emulator.
func (e *Emu) Gateway() string { return fmt.Sprintf("http://127.0.0.1:%d", e.Port) }

// LogTail returns the last n bytes of the emulator's log.
func (e *Emu) LogTail(n int) string {
	b, err := os.ReadFile(e.Log)
	if err != nil {
		return err.Error()
	}
	if len(b) > n {
		b = b[len(b)-n:]
	}
	return string(b)
}

// URL builds {scheme}://{name}.{label}.{core.}localhost:{port}.
func (e *Emu) URL(label, name, scheme string) string {
	suffix := "localhost"
	switch label {
	case "blob", "dfs", "queue", "table":
		suffix = "core.localhost"
	}
	return fmt.Sprintf("%s://%s.%s.%s:%d", scheme, name, label, suffix, e.Port)
}

// HTTPS builds an https URL for a service host.
func (e *Emu) HTTPS(label, name string) string { return e.URL(label, name, "https") }

// HTTP builds an http URL for a service host.
func (e *Emu) HTTP(label, name string) string { return e.URL(label, name, "http") }

// Host is "{name}.{label}.localhost:{port}" without a scheme.
func (e *Emu) Host(label, name string) string {
	return strings.TrimPrefix(strings.TrimPrefix(e.URL(label, name, "http"), "http://"), "https://")
}

// Name returns a unique lower-case name: prefix + random digits (letters and digits only, at most 24 characters).
func Name(prefix string) string {
	n := counter.Add(1)
	s := fmt.Sprintf("%s%d%03d", prefix, time.Now().UnixNano()%1_000_000_000_000, n%1000)
	if len(s) > 24 {
		s = s[:24]
	}
	return strings.ToLower(s)
}

// Unique returns prefix-<digits> (for names that may contain a dash, up to 60 chars).
func Unique(prefix string) string {
	n := counter.Add(1)
	return fmt.Sprintf("%s-%d-%d", prefix, time.Now().UnixNano()%1_000_000_000, n)
}

// ---- transport ----

// ProtoLog counts the HTTP protocol versions seen on responses.
type ProtoLog struct {
	mu     sync.Mutex
	Counts map[string]int
}

// Seen returns how many responses used proto (for example "HTTP/2.0").
func (p *ProtoLog) Seen(proto string) int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.Counts[proto]
}

type recordingRT struct {
	inner http.RoundTripper
	log   *ProtoLog
}

func (r recordingRT) RoundTrip(req *http.Request) (*http.Response, error) {
	resp, err := r.inner.RoundTrip(req)
	if err == nil && r.log != nil {
		r.log.mu.Lock()
		if r.log.Counts == nil {
			r.log.Counts = map[string]int{}
		}
		r.log.Counts[resp.Proto]++
		r.log.mu.Unlock()
	}
	return resp, err
}

// TLSConfig trusts the emulator's CA only (plus the system list is not needed: the emulator is the only server).
func (e *Emu) TLSConfig() *tls.Config {
	return &tls.Config{RootCAs: e.Pool, MinVersion: tls.VersionTLS12}
}

// NewTransport is what the SDKs build by default (HTTP/2 attempted over TLS), trusting the emulator's CA.
func (e *Emu) NewTransport() *http.Transport {
	return &http.Transport{
		Proxy:                 nil,
		DialContext:           (&net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}).DialContext,
		ForceAttemptHTTP2:     true,
		MaxIdleConns:          100,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   10 * time.Second,
		ExpectContinueTimeout: 1 * time.Second,
		TLSClientConfig:       e.TLSConfig(),
	}
}

// HTTP1Transport never offers h2.
func (e *Emu) HTTP1Transport() *http.Transport {
	t := e.NewTransport()
	t.ForceAttemptHTTP2 = false
	t.TLSNextProto = map[string]func(string, *tls.Conn) http.RoundTripper{}
	t.TLSClientConfig.NextProtos = []string{"http/1.1"}
	return t
}

// Client returns an http.Client with the default transport and a record of the protocols it saw.
func (e *Emu) Client() (*http.Client, *ProtoLog) {
	pl := &ProtoLog{}
	return &http.Client{Transport: recordingRT{inner: e.NewTransport(), log: pl}}, pl
}

// ClientWith wraps a transport and records protocols.
func (e *Emu) ClientWith(rt http.RoundTripper) (*http.Client, *ProtoLog) {
	pl := &ProtoLog{}
	return &http.Client{Transport: recordingRT{inner: rt, log: pl}}, pl
}

// Transport is the policy.Transporter for ClientOptions.Transport (default HTTP/2 negotiation).
func (e *Emu) Transport() policy.Transporter {
	c, _ := e.Client()
	return c
}

// ---- credentials ----

// StaticCred is a token credential that always returns the same token. The emulator accepts any bearer token.
type StaticCred struct{}

// GetToken implements azcore.TokenCredential.
func (StaticCred) GetToken(ctx context.Context, opts policy.TokenRequestOptions) (azcore.AccessToken, error) {
	return azcore.AccessToken{Token: "azemu-test-token", ExpiresOn: time.Now().Add(time.Hour)}, nil
}

// Cred returns a fixed credential.
func Cred() azcore.TokenCredential { return StaticCred{} }

// ---- storage connection strings ----

// StorageConnStr is a connection string for blob, queue and table on plain HTTP with the given account.
func (e *Emu) StorageConnStr(account, key string) string {
	return fmt.Sprintf("DefaultEndpointsProtocol=http;AccountName=%s;AccountKey=%s;BlobEndpoint=%s;QueueEndpoint=%s;TableEndpoint=%s;",
		account, key, e.HTTP("blob", account), e.HTTP("queue", account), e.HTTP("table", account))
}

// ServiceBusConnStr builds the development emulator connection string.
func (e *Emu) ServiceBusConnStr(ns string) string {
	return fmt.Sprintf("Endpoint=sb://%s.servicebus.localhost:%d;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true", ns, e.Port)
}

// EventHubsConnStr builds the development emulator connection string (no EntityPath).
func (e *Emu) EventHubsConnStr(ns string) string {
	return fmt.Sprintf("Endpoint=sb://%s.eventhub.localhost:%d;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true", ns, e.Port)
}

// ---- debugging ----

type dumpRT struct{ inner http.RoundTripper }

func (d dumpRT) RoundTrip(req *http.Request) (*http.Response, error) {
	var reqBody []byte
	if req.Body != nil && req.Body != http.NoBody {
		reqBody, _ = io.ReadAll(req.Body)
		req.Body.Close()
		saved := reqBody
		req.Body = io.NopCloser(strings.NewReader(string(saved)))
		req.GetBody = func() (io.ReadCloser, error) { return io.NopCloser(strings.NewReader(string(saved))), nil }
		if len(reqBody) > 1500 {
			reqBody = reqBody[:1500]
		}
	}
	resp, err := d.inner.RoundTrip(req)
	if err != nil {
		fmt.Fprintf(os.Stderr, "DUMP %s %s -> error %v\n", req.Method, req.URL, err)
		return resp, err
	}
	b, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	resp.Body = io.NopCloser(strings.NewReader(string(b)))
	if len(b) > 1500 {
		b = b[:1500]
	}
	fmt.Fprintf(os.Stderr, "DUMP %s %s [%s]\n  req: %s\n  -> %s %s\n  resp: %s\n", req.Method, req.URL, resp.Proto, reqBody, resp.Status, resp.Header, b)
	return resp, nil
}

// Dump wraps a transport so that every request and response is printed to stderr when AZEMU_DUMP is set.
func Dump(rt http.RoundTripper) http.RoundTripper {
	if os.Getenv("AZEMU_DUMP") == "" {
		return rt
	}
	return dumpRT{rt}
}

// Finding marks a test as a known difference of the emulator from Azure (see docs/sdk-notes.md). The test is skipped, unless the
// environment variable AZEMU_RUN_FINDINGS=1 is set: then it runs (and fails while the difference exists), which is how to
// check whether a fix in the emulator worked.
func Finding(t testing.TB, msg string) {
	t.Helper()
	if os.Getenv("AZEMU_RUN_FINDINGS") == "1" {
		return
	}
	t.Skip(msg)
}
