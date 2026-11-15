package main

import (
	"encoding/json"
	"net"
	"testing"
	"time"

	"github.com/pion/turn/v5"
)

// startLoopbackTurn spins up a relay on an ephemeral loopback port with a small
// relay range so the test does not depend on privileged or high ports.
func startLoopbackTurn(t *testing.T) *turnProvider {
	t.Helper()
	tp := newTurnProvider(turnConfig{
		addr:      "127.0.0.1:0",
		relayAddr: net.ParseIP("127.0.0.1"),
		minPort:   40000,
		maxPort:   40100,
		realm:     "127.0.0.1",
	})
	if err := tp.start(); err != nil {
		t.Fatalf("TURN start: %v", err)
	}
	t.Cleanup(tp.stop)
	return tp
}

// TestTurnRelayRoundTrip is an end-to-end check: issue a credential, then use a
// real pion/turn client to authenticate and allocate a relay through Mamla.
func TestTurnRelayRoundTrip(t *testing.T) {
	tp := startLoopbackTurn(t)
	srvAddr := net.JoinHostPort("127.0.0.1", mustPort(t, tp.udp.LocalAddr().String()))

	username, password := tp.credential("turn:" + srvAddr)
	if username == "" || password == "" {
		t.Fatal("expected credentials from the provider")
	}

	clientConn, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("client socket: %v", err)
	}
	defer clientConn.Close()

	c, err := turn.NewClient(&turn.ClientConfig{
		Conn:           clientConn,
		TURNServerAddr: srvAddr,
		Username:       username,
		Password:       password,
		Realm:          "127.0.0.1",
	})
	if err != nil {
		t.Fatalf("client: %v", err)
	}
	if err := c.Listen(); err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer c.Close()

	relay, err := c.Allocate()
	if err != nil {
		t.Fatalf("allocate: %v", err)
	}
	defer relay.Close()
	if relay.LocalAddr() == nil {
		t.Fatal("allocation returned no relayed address")
	}
}

func mustPort(t *testing.T, addr string) string {
	t.Helper()
	_, port, err := net.SplitHostPort(addr)
	if err != nil {
		t.Fatalf("port from %q: %v", addr, err)
	}
	return port
}

// TestTurnCredentialCachesUntilExpiry checks that repeated requests for the same
// turn URL reuse one credential, and a different URL gets its own.
func TestTurnCredentialCachesUntilExpiry(t *testing.T) {
	tp := newTurnProvider(turnConfig{realm: "r", minPort: 40000, maxPort: 40100})

	user1, pass1 := tp.credential("turn:turn.example.org:3478")
	if user1 == "" || pass1 == "" {
		t.Fatal("expected credentials")
	}
	user2, pass2 := tp.credential("turn:turn.example.org:3478")
	if user1 != user2 || pass1 != pass2 {
		t.Fatalf("cache miss: got %q/%q, want cached %q/%q", user2, pass2, user1, pass1)
	}
	user3, _ := tp.credential("turns:turn.example.org:5349")
	if user3 == user1 {
		t.Fatal("expected a separate credential for a different turn URL")
	}
}

// TestTurnAuthRejectsUnknownUser ensures the relay rejects credentials it never
// issued, so a leaked username cannot be reused elsewhere.
func TestTurnAuthRejectsUnknownUser(t *testing.T) {
	tp := newTurnProvider(turnConfig{realm: "r"})
	_, _, ok := tp.authHandler(&turn.RequestAttributes{Username: "never-issued"})
	if ok {
		t.Fatal("unknown user must be rejected")
	}
	tp.creds["known"] = turnCredential{password: "p", expiresAt: time.Now().Add(time.Hour)}
	if _, key, ok := tp.authHandler(&turn.RequestAttributes{Username: "known"}); !ok || len(key) == 0 {
		t.Fatal("known user must return a valid auth key")
	}
}

// TestTurnCredentialExpiry ensures an expired credential is dropped and a new one
// issued rather than reused.
func TestTurnCredentialExpiry(t *testing.T) {
	tp := newTurnProvider(turnConfig{realm: "r"})
	tp.creds["old"] = turnCredential{password: "old", expiresAt: time.Now().Add(-time.Minute)}
	user, pass := tp.credential("turn:turn.example.org:3478")
	if user == "old" {
		t.Fatal("expired credential was reused")
	}
	if pass == "old" {
		t.Fatal("expired password was served")
	}
}

func TestIceWithCredentialsInjectsOnlyIntoTurn(t *testing.T) {
	tp := newTurnProvider(turnConfig{realm: "r"})
	// Seed a deterministic cached credential without needing a running relay.
	tp.cached["turn:turn.example.org:3478"] = "u"
	tp.creds["u"] = turnCredential{password: "p", expiresAt: time.Now().Add(time.Hour)}

	v := &voiceServer{
		ice: json.RawMessage(`[
			{"urls":"stun:stun.l.google.com:19302"},
			{"urls":["turn:turn.example.org:3478","turns:turn.example.org:5349"]}
		]`),
		turn: tp,
	}

	out := v.iceWithCredentials().([]iceServer)
	if len(out) != 2 {
		t.Fatalf("servers = %d, want 2", len(out))
	}
	if out[0].Username != "" || out[0].Credential != "" {
		t.Fatalf("STUN server got credentials: %+v", out[0])
	}
	if out[1].Username != "u" || out[1].Credential != "p" {
		t.Fatalf("turn server creds = %q/%q, want u/p", out[1].Username, out[1].Credential)
	}
}

func TestIceWithCredentialsReturnsRawWhenNoProvider(t *testing.T) {
	v := &voiceServer{ice: json.RawMessage(`[{"urls":"stun:x"}]`)}
	if got := v.iceWithCredentials(); string(got.(json.RawMessage)) != string(v.ice) {
		t.Fatalf("expected raw ice passthrough, got %v", got)
	}
}

func TestTurnURLParsing(t *testing.T) {
	servers := []iceServer{
		{URLs: json.RawMessage(`"stun:one"`)},
		{URLs: json.RawMessage(`["turn:two:3478","turns:two:5349","stun:three"]`)},
	}
	got := turnURLs(servers)
	if len(got) != 2 || got[0] != "turn:two:3478" || got[1] != "turns:two:5349" {
		t.Fatalf("turnURLs = %v", got)
	}
}

func TestRealmFromTurnURLs(t *testing.T) {
	got := realmFromTurnURLs([]string{"turn:turn.example.org:3478", "turns:10.0.0.5:5349"})
	if got != "turn.example.org" {
		t.Fatalf("realm = %q, want turn.example.org", got)
	}
	if got := realmFromTurnURLs([]string{"stun:x"}); got != "" {
		t.Fatalf("realm for STUN-only = %q, want empty", got)
	}
}

func TestParsePortRange(t *testing.T) {
	min, max, err := parsePortRange("", "")
	if err != nil || min != 49152 || max != 65534 {
		t.Fatalf("default range = %d-%d, err %v", min, max, err)
	}
	min, max, err = parsePortRange("40000", "41000")
	if err != nil || min != 40000 || max != 41000 {
		t.Fatalf("custom range = %d-%d, err %v", min, max, err)
	}
	if _, _, err := parsePortRange("50000", "40000"); err == nil {
		t.Fatal("expected error for max < min")
	}
}
