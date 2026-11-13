package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

type testPeer struct {
	ID    string `json:"id"`
	Token string `json:"token"`
}

func request(t *testing.T, base, path, token string, body any, want int) []byte {
	t.Helper()
	data, err := json.Marshal(body)
	if err != nil {
		t.Fatal(err)
	}
	req, _ := http.NewRequest(http.MethodPost, base+path, bytes.NewReader(data))
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	client := &http.Client{Timeout: 3 * time.Second}
	res, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	var out bytes.Buffer
	_, _ = out.ReadFrom(res.Body)
	if res.StatusCode != want {
		t.Fatalf("%s: status %d, want %d; %s", path, res.StatusCode, want, out.String())
	}
	return out.Bytes()
}

func joinPeer(t *testing.T, base, room, name string) testPeer {
	t.Helper()
	data := request(t, base, "/api/join", "", map[string]string{"room": room, "name": name}, 201)
	var peer testPeer
	if err := json.Unmarshal(data, &peer); err != nil {
		t.Fatal(err)
	}
	if peer.ID == "" || peer.Token == "" {
		t.Fatal("missing session credentials")
	}
	return peer
}

func openEvents(t *testing.T, base string, p testPeer) *bufio.Scanner {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	t.Cleanup(cancel)
	req, _ := http.NewRequestWithContext(ctx, "GET", base+"/api/events", nil)
	req.Header.Set("Authorization", "Bearer "+p.Token)
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	if res.StatusCode != 200 {
		res.Body.Close()
		t.Fatalf("events: %d", res.StatusCode)
	}
	t.Cleanup(func() { res.Body.Close() })
	return bufio.NewScanner(res.Body)
}

func next(t *testing.T, scanner *bufio.Scanner, want string) map[string]any {
	t.Helper()
	for scanner.Scan() {
		if !strings.HasPrefix(scanner.Text(), "data: ") {
			continue
		}
		var e struct {
			Type string         `json:"type"`
			Data map[string]any `json:"data"`
		}
		if err := json.Unmarshal([]byte(strings.TrimPrefix(scanner.Text(), "data: ")), &e); err != nil {
			t.Fatal(err)
		}
		if e.Type != want {
			t.Fatalf("event = %s, want %s", e.Type, want)
		}
		return e.Data
	}
	t.Fatalf("stream ended waiting for %s: %v", want, scanner.Err())
	return nil
}

func TestRoomLifecycleAndIsolation(t *testing.T) {
	v := newVoiceServer(json.RawMessage(`[]`), nil)
	srv := httptest.NewServer(v.handler())
	defer srv.Close()
	a := joinPeer(t, srv.URL, "friends", "Аня")
	aEvents := openEvents(t, srv.URL, a)
	if peers := next(t, aEvents, "ready")["peers"].([]any); len(peers) != 0 {
		t.Fatal("first peer should be alone")
	}
	b := joinPeer(t, srv.URL, "friends", "Боря")
	bEvents := openEvents(t, srv.URL, b)
	ready := next(t, bEvents, "ready")
	if peers := ready["peers"].([]any); len(peers) != 1 || peers[0].(map[string]any)["id"] != a.ID {
		t.Fatal("newcomer needs existing peer")
	}
	if next(t, aEvents, "peer-joined")["id"] != b.ID {
		t.Fatal("missing join notification")
	}
	for _, kind := range []string{"offer", "ice", "answer"} {
		request(t, srv.URL, "/api/signal", b.Token, map[string]any{"to": a.ID, "type": kind, "data": map[string]string{"test": kind}}, 204)
		data := next(t, aEvents, "signal")
		if data["from"] != b.ID || data["type"] != kind || data["data"].(map[string]any)["test"] != kind {
			t.Fatal("signal changed in transit")
		}
	}
	request(t, srv.URL, "/api/mute", b.Token, map[string]bool{"muted": true}, 204)
	if next(t, aEvents, "peer-updated")["muted"] != true {
		t.Fatal("mute state not delivered")
	}
	c := joinPeer(t, srv.URL, "other", "Другой")
	cEvents := openEvents(t, srv.URL, c)
	if peers := next(t, cEvents, "ready")["peers"].([]any); len(peers) != 0 {
		t.Fatal("rooms leaked")
	}
	request(t, srv.URL, "/api/signal", c.Token, map[string]any{"to": a.ID, "type": "offer", "data": map[string]string{}}, 404)
	request(t, srv.URL, "/api/signal", "wrong", map[string]any{"to": a.ID, "type": "offer", "data": map[string]string{}}, 401)
	request(t, srv.URL, "/api/leave", b.Token, nil, 204)
	if next(t, aEvents, "peer-left")["id"] != b.ID {
		t.Fatal("missing leave notification")
	}
	request(t, srv.URL, "/api/mute", b.Token, map[string]bool{"muted": false}, 401)
	request(t, srv.URL, "/api/leave", a.Token, nil, 204)
	request(t, srv.URL, "/api/leave", c.Token, nil, 204)
}

func TestRoomCapacityValidationAndExpiry(t *testing.T) {
	v := newVoiceServer(json.RawMessage(`[]`), nil)
	srv := httptest.NewServer(v.handler())
	defer srv.Close()
	for i := 0; i < roomLimit; i++ {
		joinPeer(t, srv.URL, "full", "Имя")
	}
	request(t, srv.URL, "/api/join", "", map[string]string{"room": "full", "name": "Лишний"}, 409)
	request(t, srv.URL, "/api/join", "", map[string]string{"room": "../invalid", "name": "Имя"}, 400)
	request(t, srv.URL, "/api/join", "", map[string]string{"room": "valid", "name": "  "}, 400)
	v.mu.Lock()
	for _, s := range v.sessions {
		s.created = time.Now().Add(-time.Minute)
	}
	v.mu.Unlock()
	p := joinPeer(t, srv.URL, "full", "Снова")
	request(t, srv.URL, "/api/leave", p.Token, nil, 204)
}

func TestUnexpectedDisconnect(t *testing.T) {
	v := newVoiceServer(json.RawMessage(`[]`), nil)
	srv := httptest.NewServer(v.handler())
	defer srv.Close()
	a := joinPeer(t, srv.URL, "room", "А")
	events := openEvents(t, srv.URL, a)
	next(t, events, "ready")
	b := joinPeer(t, srv.URL, "room", "Б")
	ctx, cancel := context.WithCancel(context.Background())
	req, _ := http.NewRequestWithContext(ctx, "GET", srv.URL+"/api/events", nil)
	req.Header.Set("Authorization", "Bearer "+b.Token)
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		cancel()
		t.Fatal(err)
	}
	next(t, events, "peer-joined")
	cancel()
	res.Body.Close()
	if next(t, events, "peer-left")["id"] != b.ID {
		t.Fatal("disconnected peer retained")
	}
	request(t, srv.URL, "/api/leave", a.Token, nil, 204)
}

func TestOriginAndStaticFiles(t *testing.T) {
	handler := newVoiceServer(json.RawMessage(`[]`), nil).handler()
	favicon := httptest.NewRecorder()
	handler.ServeHTTP(favicon, httptest.NewRequest("GET", "/favicon.ico", nil))
	if favicon.Code != http.StatusNoContent {
		t.Fatalf("favicon: status %d", favicon.Code)
	}
	for _, path := range []string{"/", "/app.js", "/auth.js", "/style.css", "/api/config", "/api/auth/status"} {
		w := httptest.NewRecorder()
		handler.ServeHTTP(w, httptest.NewRequest("GET", path, nil))
		if w.Code != 200 || w.Body.Len() == 0 {
			t.Fatalf("%s not served", path)
		}
	}
	r := httptest.NewRequest("POST", "/api/join", strings.NewReader(`{"room":"a","name":"b"}`))
	r.Header.Set("Origin", "https://foreign.invalid")
	w := httptest.NewRecorder()
	handler.ServeHTTP(w, r)
	if w.Code != 403 {
		t.Fatal("cross-origin mutation allowed")
	}
}

func TestHTTP2EventsSurviveIdleWriteDeadline(t *testing.T) {
	v := newVoiceServer(json.RawMessage(`[]`), nil)
	srv := httptest.NewUnstartedServer(v.handler())
	srv.EnableHTTP2 = true
	srv.StartTLS()
	defer srv.Close()
	client := srv.Client()
	res, err := client.Post(srv.URL+"/api/join", "application/json", strings.NewReader(`{"room":"idle-http2","name":"Test"}`))
	if err != nil {
		t.Fatal(err)
	}
	var peer testPeer
	err = json.NewDecoder(res.Body).Decode(&peer)
	res.Body.Close()
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 22*time.Second)
	defer cancel()
	req, _ := http.NewRequestWithContext(ctx, "GET", srv.URL+"/api/events", nil)
	req.Header.Set("Authorization", "Bearer "+peer.Token)
	res, err = client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	if res.ProtoMajor != 2 {
		t.Fatalf("expected HTTP/2, got %s", res.Proto)
	}
	scanner := bufio.NewScanner(res.Body)
	next(t, scanner, "ready")
	// The first keepalive follows 15 seconds of silence. A 10-second write
	// deadline must limit only an active write, not expire the idle H2 stream.
	for scanner.Scan() {
		if scanner.Text() == ": ping" {
			return
		}
	}
	t.Fatalf("HTTP/2 stream closed before its first heartbeat: %v", scanner.Err())
}
