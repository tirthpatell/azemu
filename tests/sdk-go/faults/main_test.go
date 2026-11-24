// Package faults: the SDK clients of Go against fault rules (`docs/faults.md`): they retry what the emulator throttles or drops,
// wait as long as the retry header says, and raise the right error when retries are off.
package faults

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"testing"

	"azemu-sdk-go/internal/emu"
)

func TestMain(m *testing.M) { emu.Main(m) }

// rule is one fault rule as the admin API takes it.
type rule map[string]any

// addRule adds a rule and returns its id. Every test starts with no rules: it removes the ones it added when it ends.
func addRule(t testing.TB, r rule) int {
	t.Helper()
	body, err := json.Marshal(r)
	if err != nil {
		t.Fatal(err)
	}
	resp, err := http.Post(emu.E.Gateway()+"/_azemu/faults", "application/json", bytes.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	text, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 201 {
		t.Fatalf("adding a rule: %d %s", resp.StatusCode, text)
	}
	var got struct {
		ID int `json:"id"`
	}
	if err := json.Unmarshal(text, &got); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { removeRule(got.ID) })
	return got.ID
}

func removeRule(id int) {
	req, _ := http.NewRequest(http.MethodDelete, fmt.Sprintf("%s/_azemu/faults/%d", emu.E.Gateway(), id), nil)
	if resp, err := http.DefaultClient.Do(req); err == nil {
		resp.Body.Close()
	}
}

// fired is how many times the rule has fired.
func fired(t testing.TB, id int) int {
	t.Helper()
	resp, err := http.Get(emu.E.Gateway() + "/_azemu/faults")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var list struct {
		Rules []struct {
			ID    int `json:"id"`
			Fired int `json:"fired"`
		} `json:"rules"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&list); err != nil {
		t.Fatal(err)
	}
	for _, r := range list.Rules {
		if r.ID == id {
			return r.Fired
		}
	}
	t.Fatalf("no rule %d", id)
	return 0
}
