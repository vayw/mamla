package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/cookiejar"
	"net/http/httptest"
	"net/http/httputil"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

type accessResponse struct {
	status  int
	body    []byte
	headers http.Header
	cookies []*http.Cookie
}

func accessRequest(t *testing.T, client *http.Client, base, path string, body any, key, roomToken string) accessResponse {
	t.Helper()
	method := "GET"
	var data io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			t.Fatal(err)
		}
		data = bytes.NewReader(encoded)
		method = "POST"
	}
	req, _ := http.NewRequest(method, base+path, data)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if key != "" {
		req.Header.Set("X-Mamla-Key", key)
	}
	if roomToken != "" {
		req.Header.Set("Authorization", "Bearer "+roomToken)
	}
	res, err := client.Do(req)
	if err != nil {
		t.Fatalf("request failed: %s", path)
	}
	defer res.Body.Close()
	content, _ := io.ReadAll(res.Body)
	return accessResponse{res.StatusCode, content, res.Header, res.Cookies()}
}
func accessStatus(t *testing.T, response accessResponse, status int) {
	t.Helper()
	if response.status != status {
		t.Fatalf("status %d, want %d", response.status, status)
	}
}
func authFixture(t *testing.T) (*voiceServer, *httptest.Server, *http.Client) {
	t.Helper()
	v := newVoiceServer(json.RawMessage(`[]`))
	v.auth = &authStore{path: filepath.Join(t.TempDir(), "auth.json")}
	v.heartbeat = 20 * time.Millisecond
	srv := httptest.NewTLSServer(v.handler())
	t.Cleanup(srv.Close)
	client := *srv.Client()
	client.Jar, _ = cookiejar.New(nil)
	return v, srv, &client
}
func acceptAccess(t *testing.T, store *authStore, client *http.Client, base, name string) string {
	t.Helper()
	invite, err := store.invite(name, time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	key := "mk_" + randomID(32)
	response := accessRequest(t, client, base, "/api/auth/accept", map[string]string{"invite": invite, "key": key}, "", "")
	accessStatus(t, response, 204)
	if len(response.cookies) != 1 {
		t.Fatal("missing access cookie")
	}
	cookie := response.cookies[0]
	if cookie.Name != authCookie || !cookie.HttpOnly || !cookie.Secure || cookie.Path != "/" || cookie.Domain != "" || cookie.SameSite != http.SameSiteLaxMode {
		t.Fatal("unsafe cookie attributes")
	}
	return key
}

func TestInvitationAtomicAcrossStoreInstances(t *testing.T) {
	path := filepath.Join(t.TempDir(), "auth.json")
	store := &authStore{path: path}
	invite, err := store.invite("Friend", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	var successes atomic.Int32
	var group sync.WaitGroup
	keys := make([]string, 12)
	for i := range keys {
		keys[i] = "mk_" + randomID(32)
		group.Add(1)
		go func(key string) {
			defer group.Done()
			other := &authStore{path: path}
			if _, err := other.accept(invite, key, ""); err == nil {
				successes.Add(1)
			} else if !errors.Is(err, errAccess) {
				t.Error("unexpected storage error")
			}
		}(keys[i])
	}
	group.Wait()
	if successes.Load() != 1 {
		t.Fatalf("invitation redeemed %d times", successes.Load())
	}
	content, _ := os.ReadFile(path)
	if bytes.Contains(content, []byte(invite)) {
		t.Fatal("raw invitation stored")
	}
	for _, key := range keys {
		if bytes.Contains(content, []byte(key)) {
			t.Fatal("raw key stored")
		}
	}
	info, _ := os.Stat(path)
	if info.Mode().Perm() != 0600 {
		t.Fatal("auth file is not private")
	}
	if err := store.data(false, func(db *accessData) error {
		if len(db.Users) != 1 || len(db.Invites) != 1 || !db.Invites[0].Used {
			t.Error("incorrect redemption state")
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
}

func TestAccessBrowserNativeAndRoomOwnership(t *testing.T) {
	v, srv, browser := authFixture(t)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/", nil, "", ""), 200)
	unauthorized := accessRequest(t, browser, srv.URL, "/api/config", nil, "", "")
	accessStatus(t, unauthorized, 401)
	if unauthorized.headers.Get("WWW-Authenticate") == "" {
		t.Fatal("missing authentication challenge")
	}
	key := acceptAccess(t, v.auth, browser, srv.URL, "Friend")
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 200)
	joined := accessRequest(t, browser, srv.URL, "/api/join", map[string]string{"room": "friends", "name": "One"}, "", "")
	accessStatus(t, joined, 201)
	var room testPeer
	if err := json.Unmarshal(joined.body, &room); err != nil {
		t.Fatal(err)
	}
	// Another service user may not control a call by presenting its stolen token.
	other := *srv.Client()
	other.Jar, _ = cookiejar.New(nil)
	acceptAccess(t, v.auth, &other, srv.URL, "Other")
	accessStatus(t, accessRequest(t, &other, srv.URL, "/api/mute", map[string]bool{"muted": true}, "", room.Token), 403)
	// The native client uses its personal key alongside the room Bearer token.
	native := *srv.Client()
	native.Jar = nil
	accessStatus(t, accessRequest(t, &native, srv.URL, "/api/config", nil, key, ""), 200)
	accessStatus(t, accessRequest(t, &native, srv.URL, "/api/mute", map[string]bool{"muted": true}, key, room.Token), 204)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/auth/logout", map[string]string{}, "", ""), 204)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 401)
	// A saved key permits a new browser session after logout.
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/auth/login", map[string]string{"key": key}, "", ""), 204)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 200)
	if err := (&authStore{path: v.auth.path}).revoke("Friend"); err != nil {
		t.Fatal(err)
	}
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 401)
	accessStatus(t, accessRequest(t, &native, srv.URL, "/api/config", nil, key, ""), 401)
}

func TestRevocationClosesExistingEventStream(t *testing.T) {
	v, srv, browser := authFixture(t)
	acceptAccess(t, v.auth, browser, srv.URL, "Friend")
	joined := accessRequest(t, browser, srv.URL, "/api/join", map[string]string{"room": "friends", "name": "One"}, "", "")
	accessStatus(t, joined, 201)
	var room testPeer
	_ = json.Unmarshal(joined.body, &room)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	req, _ := http.NewRequestWithContext(ctx, "GET", srv.URL+"/api/events", nil)
	req.Header.Set("Authorization", "Bearer "+room.Token)
	response, err := browser.Do(req)
	if err != nil {
		t.Fatal("events request failed")
	}
	defer response.Body.Close()
	scanner := bufio.NewScanner(response.Body)
	next(t, scanner, "ready")
	if err := v.auth.revoke("Friend"); err != nil {
		t.Fatal(err)
	}
	for scanner.Scan() {
	}
	if ctx.Err() != nil {
		t.Fatal("revocation did not close SSE")
	}
	v.mu.Lock()
	remaining := len(v.sessions)
	v.mu.Unlock()
	if remaining != 0 {
		t.Fatal("revoked call session retained")
	}
}

func TestBrowserSessionExpiresButSavedPersonalKeyStillWorks(t *testing.T) {
	v, srv, browser := authFixture(t)
	key := acceptAccess(t, v.auth, browser, srv.URL, "Friend")
	if err := v.auth.data(true, func(db *accessData) error {
		for i := range db.Sessions {
			db.Sessions[i].Expires = time.Now().Add(-time.Minute)
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 401)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/auth/login", map[string]string{"key": key}, "", ""), 204)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 200)
}

func TestLoginRotatesPreviousBrowserSession(t *testing.T) {
	v, srv, browser := authFixture(t)
	key := acceptAccess(t, v.auth, browser, srv.URL, "Friend")
	base, _ := url.Parse(srv.URL)
	oldJar, _ := cookiejar.New(nil)
	oldJar.SetCookies(base, browser.Jar.Cookies(base))
	oldBrowser := *srv.Client()
	oldBrowser.Jar = oldJar
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/auth/login", map[string]string{"key": key}, "", ""), 204)
	accessStatus(t, accessRequest(t, &oldBrowser, srv.URL, "/api/config", nil, "", ""), 401)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 200)
}

func TestLogoutDoesNotHideStorageFailure(t *testing.T) {
	v, srv, browser := authFixture(t)
	acceptAccess(t, v.auth, browser, srv.URL, "Friend")
	if err := os.Chmod(v.auth.path, 0644); err != nil {
		t.Fatal(err)
	}
	failed := accessRequest(t, browser, srv.URL, "/api/auth/logout", map[string]string{}, "", "")
	accessStatus(t, failed, 503)
	if len(failed.cookies) != 0 {
		t.Fatal("logout acknowledged without server-side invalidation")
	}
	if err := os.Chmod(v.auth.path, 0600); err != nil {
		t.Fatal(err)
	}
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 200)
	// Browser logout selects the cookie even if a native-key header is also sent.
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/auth/logout", map[string]string{}, "invalid", ""), 204)
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 401)
}

func TestInvitePreviewExpiryAndCSRF(t *testing.T) {
	v, srv, browser := authFixture(t)
	invite, err := v.auth.invite("Friend", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	// Fragments never reach the server; a preview GET must not redeem anything.
	accessStatus(t, accessRequest(t, browser, srv.URL, "/#invite="+invite, nil, "", ""), 200)
	key := "mk_" + randomID(32)
	for _, origin := range []string{"https://foreign.invalid", ""} {
		body, _ := json.Marshal(map[string]string{"invite": invite, "key": key})
		req, _ := http.NewRequest("POST", srv.URL+"/api/auth/accept", bytes.NewReader(body))
		req.Header.Set("Origin", origin)
		req.Header.Set("Content-Type", "text/plain")
		res, err := browser.Do(req)
		if err != nil {
			t.Fatal("csrf request failed")
		}
		res.Body.Close()
		if res.StatusCode != 403 && res.StatusCode != 415 {
			t.Fatal("unsafe cross-site request accepted")
		}
	}
	if err := v.auth.data(true, func(db *accessData) error {
		if db.Invites[0].Used {
			t.Error("preview or form consumed invitation")
		}
		db.Invites[0].Expires = time.Now().Add(-time.Minute)
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/auth/accept", map[string]string{"invite": invite, "key": key}, "", ""), 401)
	// An unreadable/insecure store is an outage, never an open service.
	if err := os.Chmod(v.auth.path, 0644); err != nil {
		t.Fatal(err)
	}
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 503)
	if err := os.Chmod(v.auth.path, 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(v.auth.path, []byte(`{}`), 0600); err != nil {
		t.Fatal(err)
	}
	accessStatus(t, accessRequest(t, browser, srv.URL, "/api/config", nil, "", ""), 503)
}

func TestInviteCLIAndSecretFreeListing(t *testing.T) {
	path := filepath.Join(t.TempDir(), "auth.json")
	t.Setenv("AUTH_FILE", path)
	t.Setenv("PUBLIC_URL", "https://mamla.example")
	var output bytes.Buffer
	if handled, err := authCLI([]string{"invite", "create", "Friend", "--expires", "24h"}, &output); !handled || err != nil {
		t.Fatal("invite CLI failed")
	}
	link, err := url.Parse(strings.TrimSpace(output.String()))
	if err != nil || link.Scheme != "https" || link.RawQuery != "" || !strings.HasPrefix(link.Fragment, "invite=mi_") {
		t.Fatal("bad invite URL")
	}
	secret := strings.TrimPrefix(link.Fragment, "invite=")
	output.Reset()
	if _, err := authCLI([]string{"access", "list"}, &output); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(output.String(), secret) || !strings.Contains(output.String(), "Friend") {
		t.Fatal("listing exposed secret or missed label")
	}
	if _, err := authCLI([]string{"access", "revoke", "Friend"}, &output); err != nil {
		t.Fatal(err)
	}
	if _, err := (&authStore{path: path}).accept(secret, "mk_"+randomID(32), ""); !errors.Is(err, errAccess) {
		t.Fatal("revoked invite accepted")
	}
	t.Setenv("PUBLIC_URL", "http://mamla.example")
	if _, err := authCLI([]string{"invite", "create", "Other"}, &output); err == nil {
		t.Fatal("insecure public invitation URL accepted")
	}
}

func TestAccessBehindTLSTerminatorWithHTTPUpstream(t *testing.T) {
	v := newVoiceServer(json.RawMessage(`[]`))
	v.auth = &authStore{path: filepath.Join(t.TempDir(), "auth.json")}
	upstream := httptest.NewServer(v.handler())
	defer upstream.Close()
	upstreamURL, _ := url.Parse(upstream.URL)
	public := httptest.NewTLSServer(httputil.NewSingleHostReverseProxy(upstreamURL))
	defer public.Close()
	browser := public.Client()
	browser.Jar, _ = cookiejar.New(nil)
	invite, err := v.auth.invite("Friend", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	body, _ := json.Marshal(map[string]string{"invite": invite, "key": "mk_" + randomID(32)})
	req, _ := http.NewRequest("POST", public.URL+"/api/auth/accept", bytes.NewReader(body))
	req.Header.Set("Origin", public.URL)
	req.Header.Set("Content-Type", "application/json")
	res, err := browser.Do(req)
	if err != nil {
		t.Fatal("proxy accept failed")
	}
	res.Body.Close()
	if res.StatusCode != 204 {
		t.Fatalf("proxy accept status %d", res.StatusCode)
	}
	accessStatus(t, accessRequest(t, browser, public.URL, "/api/config", nil, "", ""), 200)
}
