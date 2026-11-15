package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/pion/turn/v5"
)

// turnConfig configures the in-process pion/turn relay server that Mamla hosts,
// removing the need for an external coturn. relayAddr is the public IP that
// clients must be able to reach; the relay UDP ports (minPort..maxPort) must be
// opened in the host firewall.
type turnConfig struct {
	addr      string   // UDP listen address, e.g. "0.0.0.0:3478"
	tcpAddr   string   // TCP listen address; "" disables TCP relay
	relayAddr net.IP   // public IP advertised to clients as the relay address
	minPort   uint16   // lowest UDP relay port (inclusive)
	maxPort   uint16   // highest UDP relay port (inclusive)
	realm     string   // must match the host clients use in their turn: URL
	ttl       time.Duration
}

// turnCredential is a stored, short-lived credential.
type turnCredential struct {
	password  string
	expiresAt time.Time
}

// turnProvider runs a pion/turn relay server in-process and issues the
// short-lived username/password credentials that /api/config hands to the
// browser. The credential store is shared with the relay's auth handler, so a
// credential issued for a config request is the one the relay validates.
type turnProvider struct {
	cfg turnConfig

	udp net.PacketConn
	tcp net.Listener
	srv *turn.Server

	mu     sync.Mutex
	creds  map[string]turnCredential // username -> stored credential
	cached map[string]string         // normalized turn URL -> issued username
}

func newTurnProvider(cfg turnConfig) *turnProvider {
	if cfg.ttl <= 0 {
		cfg.ttl = 24 * time.Hour
	}
	if cfg.realm == "" {
		cfg.realm = "mamla.local"
	}
	if cfg.minPort == 0 || cfg.maxPort == 0 || cfg.maxPort < cfg.minPort {
		cfg.minPort, cfg.maxPort = 49152, 65534
	}
	return &turnProvider{
		cfg:    cfg,
		creds:  make(map[string]turnCredential),
		cached: make(map[string]string),
	}
}

// start binds the listeners and launches the pion/turn server.
func (tp *turnProvider) start() error {
	udp, err := net.ListenPacket("udp4", tp.cfg.addr)
	if err != nil {
		return fmt.Errorf("TURN: bind UDP %s: %w", tp.cfg.addr, err)
	}

	relayGen := &turn.RelayAddressGeneratorPortRange{
		RelayAddress: tp.cfg.relayAddr,
		Address:      "0.0.0.0",
		MinPort:      tp.cfg.minPort,
		MaxPort:      tp.cfg.maxPort,
	}

	cfg := turn.ServerConfig{
		Realm:             tp.cfg.realm,
		AuthHandler:       tp.authHandler,
		PacketConnConfigs: []turn.PacketConnConfig{{PacketConn: udp, RelayAddressGenerator: relayGen}},
	}

	if tp.cfg.tcpAddr != "" {
		tcp, err := net.Listen("tcp", tp.cfg.tcpAddr)
		if err != nil {
			_ = udp.Close()
			tp.udp = nil
			return fmt.Errorf("TURN: bind TCP %s: %w", tp.cfg.tcpAddr, err)
		}
		cfg.ListenerConfigs = []turn.ListenerConfig{{Listener: tcp, RelayAddressGenerator: relayGen}}
	}

	srv, err := turn.NewServer(cfg)
	if err != nil {
		_ = udp.Close()
		tp.udp = nil
		return fmt.Errorf("TURN: server: %w", err)
	}

	tp.udp = udp
	tp.srv = srv
	return nil
}

// stop tears down the relay server and its listeners.
func (tp *turnProvider) stop() {
	if tp.srv != nil {
		tp.srv.Close()
		tp.srv = nil
	}
	if tp.udp != nil {
		tp.udp.Close()
		tp.udp = nil
	}
	if tp.tcp != nil {
		tp.tcp.Close()
		tp.tcp = nil
	}
}

// authHandler answers the relay's 4.2 challenge with the long-term key for the
// stored username, or rejects unknown/expired users. The key is derived the
// same way the browser does it from the username/password /api/config served.
func (tp *turnProvider) authHandler(ra *turn.RequestAttributes) (string, []byte, bool) {
	tp.mu.Lock()
	defer tp.mu.Unlock()
	c, ok := tp.creds[ra.Username]
	if !ok || !time.Now().Before(c.expiresAt) {
		return "", nil, false
	}
	return ra.Username, turn.GenerateAuthKey(ra.Username, tp.cfg.realm, c.password), true
}

// credential returns a fresh or cached username/password for the given turn URL.
// It never errors: if issuing fails the browser simply falls back to any STUN
// entries that still work.
func (tp *turnProvider) credential(turnURL string) (username, credential string) {
	key := strings.ToLower(strings.TrimSpace(turnURL))
	if key == "" {
		return "", ""
	}
	tp.mu.Lock()
	defer tp.mu.Unlock()
	if u, ok := tp.cached[key]; ok {
		if c, ok := tp.creds[u]; ok && time.Now().Before(c.expiresAt) {
			return u, c.password
		}
	}
	// Keep the store bounded: sweep expired credentials before issuing a new one.
	if len(tp.creds) > 512 {
		now := time.Now()
		for u, c := range tp.creds {
			if !now.Before(c.expiresAt) {
				delete(tp.creds, u)
				delete(tp.cached, u)
			}
		}
	}

	username = randomID(16)
	password := randomID(24)
	tp.creds[username] = turnCredential{password: password, expiresAt: time.Now().Add(tp.cfg.ttl)}
	tp.cached[key] = username
	return username, password
}

// resolveRelayIP picks the IP advertised as the relay address. Clients must be
// able to reach it, so an explicit config wins; otherwise probe the host's
// primary outbound address and fall back to loopback for local use.
func resolveRelayIP(explicit string) net.IP {
	if explicit != "" {
		if ip := net.ParseIP(explicit); ip != nil {
			return ip
		}
		log.Printf("TURN: ignoring invalid TURN_RELAY_IP %q", explicit)
	}
	if ip := primaryOutboundIP(); ip != nil {
		return ip
	}
	log.Printf("TURN: could not detect a relay IP, falling back to 127.0.0.1 (relay works on loopback only)")
	return net.ParseIP("127.0.0.1")
}

// primaryOutboundIP returns the local address the host would use to reach the
// internet. UDP "connect" sends no packets, so this is cheap; a dialer timeout
// keeps it from hanging when outbound is blocked.
func primaryOutboundIP() net.IP {
	d := net.Dialer{Timeout: 2 * time.Second}
	conn, err := d.Dial("udp", "9.9.9.9:53")
	if err != nil {
		return nil
	}
	defer conn.Close()
	if addr, ok := conn.LocalAddr().(*net.UDPAddr); ok {
		return addr.IP
	}
	return nil
}

// parsePortRange reads two optional port strings, defaulting to 49152/65534,
// and validates the range.
func parsePortRange(minS, maxS string) (uint16, uint16, error) {
	minPort, maxPort := uint16(49152), uint16(65534)
	if minS != "" {
		v, err := strconv.Atoi(minS)
		if err != nil || v <= 0 || v > 65535 {
			return 0, 0, fmt.Errorf("TURN_MIN_PORT must be between 1 and 65535")
		}
		minPort = uint16(v)
	}
	if maxS != "" {
		v, err := strconv.Atoi(maxS)
		if err != nil || v <= 0 || v > 65535 {
			return 0, 0, fmt.Errorf("TURN_MAX_PORT must be between 1 and 65535")
		}
		maxPort = uint16(v)
	}
	if maxPort < minPort {
		return 0, 0, fmt.Errorf("TURN_MAX_PORT must be >= TURN_MIN_PORT")
	}
	return minPort, maxPort, nil
}

// realmFromTurnURLs derives a default realm from the host of the first turn:
// URL, matching what a browser uses as the TURN realm.
func realmFromTurnURLs(urls []string) string {
	for _, u := range urls {
		if !isTurnURL(u) {
			continue
		}
		host := u
		if i := strings.IndexByte(host, ':'); i >= 0 {
			host = host[i+1:]
		}
		if h, _, err := net.SplitHostPort(host); err == nil {
			return h
		}
		return strings.Trim(host, "[]")
	}
	return ""
}

// iceServer mirrors RTCIceServer. URLs may be a single string or an array.
type iceServer struct {
	URLs       json.RawMessage `json:"urls"`
	Username   string          `json:"username"`
	Credential string          `json:"credential"`
}

// unmarshalURLs accepts either "urls":"turn:x" or "urls":["turn:x","turns:x"].
func unmarshalURLs(raw json.RawMessage) []string {
	if len(raw) == 0 {
		return nil
	}
	var arr []string
	if err := json.Unmarshal(raw, &arr); err == nil {
		return arr
	}
	var s string
	if err := json.Unmarshal(raw, &s); err == nil && s != "" {
		return []string{s}
	}
	return nil
}

func isTurnURL(u string) bool {
	lower := strings.ToLower(strings.TrimSpace(u))
	return strings.HasPrefix(lower, "turn:") || strings.HasPrefix(lower, "turns:")
}

// turnURLs returns every turn:/turns: URL across the configured servers.
func turnURLs(servers []iceServer) []string {
	var out []string
	for _, s := range servers {
		for _, u := range unmarshalURLs(s.URLs) {
			if isTurnURL(u) {
				out = append(out, u)
			}
		}
	}
	return out
}

// iceWithCredentials returns the ICE config with short-lived credentials injected
// into every turn:/turns: entry. Non-turn servers are returned unchanged, and
// when no relay provider is configured v.ice is returned as-is.
func (v *voiceServer) iceWithCredentials() any {
	if v.turn == nil {
		return v.ice
	}
	var servers []iceServer
	if err := json.Unmarshal(v.ice, &servers); err != nil {
		return v.ice
	}
	for i := range servers {
		for _, u := range unmarshalURLs(servers[i].URLs) {
			if isTurnURL(u) {
				username, credential := v.turn.credential(u)
				servers[i].Username = username
				servers[i].Credential = credential
				break
			}
		}
	}
	return servers
}
