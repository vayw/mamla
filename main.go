package main

import (
	"crypto/rand"
	"embed"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"regexp"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

//go:embed web/*
var assets embed.FS

const roomLimit = 6

var roomPattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,63}$`)

type event struct {
	Type string `json:"type"`
	Data any    `json:"data"`
}

type participant struct {
	ID    string `json:"id"`
	Name  string `json:"name"`
	Muted bool   `json:"muted"`
}

type session struct {
	participant
	room, token string
	created     time.Time
	streaming   bool
	events      chan event
	done        chan struct{}
	once        sync.Once
}

func (s *session) stop() { s.once.Do(func() { close(s.done) }) }

func (s *session) send(e event) {
	select {
	case s.events <- e:
	default:
		// Disconnect slow consumers rather than silently losing SDP or ICE.
		s.stop()
	}
}

type voiceServer struct {
	mu       sync.Mutex
	sessions map[string]*session
	ice      json.RawMessage
}

func newVoiceServer(ice json.RawMessage) *voiceServer {
	return &voiceServer{sessions: make(map[string]*session), ice: ice}
}

func randomID(bytes int) string {
	b := make([]byte, bytes)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return base64.RawURLEncoding.EncodeToString(b)
}

func respond(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}

func fail(w http.ResponseWriter, status int, message string) {
	respond(w, status, map[string]string{"error": message})
}

func decode(w http.ResponseWriter, r *http.Request, dst any) bool {
	r.Body = http.MaxBytesReader(w, r.Body, 64<<10)
	d := json.NewDecoder(r.Body)
	d.DisallowUnknownFields()
	if err := d.Decode(dst); err != nil {
		fail(w, http.StatusBadRequest, "Некорректный запрос")
		return false
	}
	if err := d.Decode(new(any)); err != io.EOF {
		fail(w, http.StatusBadRequest, "Ожидался один JSON-объект")
		return false
	}
	return true
}

// All room membership and signaling operations are serialized under mu.
func (v *voiceServer) broadcast(room string, except string, e event) {
	for _, s := range v.sessions {
		if s.room == room && s.ID != except && s.streaming {
			s.send(e)
		}
	}
}

func (v *voiceServer) remove(s *session) {
	if v.sessions[s.token] != s {
		return
	}
	delete(v.sessions, s.token)
	s.stop()
	if s.streaming {
		v.broadcast(s.room, s.ID, event{"peer-left", map[string]string{"id": s.ID}})
	}
}

func (v *voiceServer) join(w http.ResponseWriter, r *http.Request) {
	var input struct {
		Room string `json:"room"`
		Name string `json:"name"`
	}
	if !decode(w, r, &input) {
		return
	}
	input.Name = strings.TrimSpace(input.Name)
	if !roomPattern.MatchString(input.Room) || utf8.RuneCountInString(input.Name) < 1 || utf8.RuneCountInString(input.Name) > 32 {
		fail(w, 400, "Нужно имя до 32 символов и корректный ID комнаты")
		return
	}
	v.mu.Lock()
	defer v.mu.Unlock()
	count := 0
	for _, s := range v.sessions {
		if !s.streaming && time.Since(s.created) > 30*time.Second {
			v.remove(s)
			continue
		}
		if s.room == input.Room {
			count++
		}
	}
	if count >= roomLimit {
		fail(w, 409, "Комната заполнена: максимум 6 участников")
		return
	}
	if len(v.sessions) >= 256 {
		fail(w, 503, "Сервер заполнен, попробуйте позже")
		return
	}
	s := &session{participant: participant{ID: randomID(12), Name: input.Name}, room: input.Room,
		token: randomID(32), created: time.Now(), events: make(chan event, 128), done: make(chan struct{})}
	v.sessions[s.token] = s
	respond(w, 201, map[string]any{"id": s.ID, "token": s.token})
}

func (v *voiceServer) authenticated(w http.ResponseWriter, r *http.Request, fn func(*session)) {
	token := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	v.mu.Lock()
	s := v.sessions[token]
	if s == nil || (!s.streaming && time.Since(s.created) > 30*time.Second) {
		v.mu.Unlock()
		fail(w, 401, "Сессия завершена, войдите снова")
		return
	}
	// The callback owns this lock, including releasing it for a streaming response.
	fn(s)
}

func (v *voiceServer) stream(w http.ResponseWriter, r *http.Request, s *session) {
	if s.streaming {
		v.mu.Unlock()
		fail(w, 409, "Сессия уже подключена")
		return
	}
	s.streaming = true
	peers := []participant{}
	for _, p := range v.sessions {
		if p.room == s.room && p.ID != s.ID && p.streaming {
			peers = append(peers, p.participant)
		}
	}
	s.send(event{"ready", map[string]any{"peers": peers}})
	v.broadcast(s.room, s.ID, event{"peer-joined", s.participant})
	v.mu.Unlock()
	defer func() { v.mu.Lock(); v.remove(s); v.mu.Unlock() }()
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("X-Accel-Buffering", "no")
	controller := http.NewResponseController(w)
	heartbeat := time.NewTicker(15 * time.Second)
	defer heartbeat.Stop()
	write := func(data string) bool {
		_ = controller.SetWriteDeadline(time.Now().Add(10 * time.Second))
		// HTTP/2 expires the stream when a write deadline elapses, even if
		// the previous write completed. Bound writes, not the idle interval.
		defer controller.SetWriteDeadline(time.Time{})
		if _, err := io.WriteString(w, data); err != nil {
			return false
		}
		return controller.Flush() == nil
	}
	for {
		select {
		case <-r.Context().Done():
			return
		case <-s.done:
			return
		case e := <-s.events:
			data, _ := json.Marshal(e)
			if !write("data: " + string(data) + "\n\n") {
				return
			}
		case <-heartbeat.C:
			if !write(": ping\n\n") {
				return
			}
		}
	}
}

func (v *voiceServer) handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /favicon.ico", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	})
	mux.HandleFunc("GET /api/config", func(w http.ResponseWriter, r *http.Request) {
		respond(w, 200, map[string]any{"iceServers": v.ice, "roomLimit": roomLimit})
	})
	mux.HandleFunc("POST /api/join", v.join)
	mux.HandleFunc("GET /api/events", func(w http.ResponseWriter, r *http.Request) {
		v.authenticated(w, r, func(s *session) { v.stream(w, r, s) })
	})
	mux.HandleFunc("POST /api/leave", func(w http.ResponseWriter, r *http.Request) {
		v.authenticated(w, r, func(s *session) {
			defer v.mu.Unlock()
			v.remove(s)
			w.WriteHeader(204)
		})
	})
	mux.HandleFunc("POST /api/mute", func(w http.ResponseWriter, r *http.Request) {
		var input struct {
			Muted *bool `json:"muted"`
		}
		if !decode(w, r, &input) {
			return
		}
		if input.Muted == nil {
			fail(w, 400, "Нужно поле muted")
			return
		}
		v.authenticated(w, r, func(s *session) {
			defer v.mu.Unlock()
			s.Muted = *input.Muted
			v.broadcast(s.room, s.ID, event{"peer-updated", s.participant})
			w.WriteHeader(204)
		})
	})
	mux.HandleFunc("POST /api/signal", func(w http.ResponseWriter, r *http.Request) {
		var input struct {
			To   string          `json:"to"`
			Type string          `json:"type"`
			Data json.RawMessage `json:"data"`
		}
		if !decode(w, r, &input) {
			return
		}
		if (input.Type != "offer" && input.Type != "answer" && input.Type != "ice") || len(input.Data) == 0 {
			fail(w, 400, "Неизвестный сигнал")
			return
		}
		v.authenticated(w, r, func(s *session) {
			defer v.mu.Unlock()
			if s.streaming {
				for _, target := range v.sessions {
					if target.ID == input.To && target.ID != s.ID && target.room == s.room && target.streaming {
						target.send(event{"signal", map[string]any{"from": s.ID, "type": input.Type, "data": input.Data}})
						w.WriteHeader(204)
						return
					}
				}
			}
			fail(w, 404, "Участник отключился")
		})
	})
	for path, file := range map[string]string{"/": "index.html", "/app.js": "app.js", "/style.css": "style.css"} {
		mux.HandleFunc("GET "+path, func(w http.ResponseWriter, r *http.Request) {
			if r.URL.Path != path {
				http.NotFound(w, r)
				return
			}
			body, _ := assets.ReadFile("web/" + file)
			contentType := map[string]string{"index.html": "text/html; charset=utf-8", "app.js": "text/javascript; charset=utf-8", "style.css": "text/css; charset=utf-8"}
			w.Header().Set("Content-Type", contentType[file])
			_, _ = w.Write(body)
		})
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Cache-Control", "no-store")
		w.Header().Set("X-Content-Type-Options", "nosniff")
		w.Header().Set("Referrer-Policy", "no-referrer")
		w.Header().Set("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; media-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")
		// Browser mutations must originate on this host. No CORS is enabled.
		if origin := r.Header.Get("Origin"); origin != "" && origin != "http://"+r.Host && origin != "https://"+r.Host {
			fail(w, 403, "Другой origin запрещён")
			return
		}
		mux.ServeHTTP(w, r)
	})
}

func main() {
	ice := json.RawMessage(`[{"urls":"stun:stun.l.google.com:19302"}]`)
	if configured := os.Getenv("ICE_SERVERS"); configured != "" {
		var parsed []struct {
			URLs any `json:"urls"`
		}
		if err := json.Unmarshal([]byte(configured), &parsed); err != nil || parsed == nil {
			log.Fatal("ICE_SERVERS must be a JSON array of RTCIceServer objects")
		}
		ice = json.RawMessage(configured)
	}
	addr := os.Getenv("ADDR")
	if addr == "" {
		addr = "127.0.0.1:8080"
	}
	certFile, keyFile := os.Getenv("TLS_CERT_FILE"), os.Getenv("TLS_KEY_FILE")
	if (certFile == "") != (keyFile == "") {
		log.Fatal("TLS_CERT_FILE and TLS_KEY_FILE must be set together")
	}
	server := &http.Server{Addr: addr, Handler: newVoiceServer(ice).handler(), ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 15 * time.Second, IdleTimeout: 60 * time.Second, MaxHeaderBytes: 16 << 10}
	var err error
	if certFile != "" {
		fmt.Printf("Mamla: https://%s\n", addr)
		err = server.ListenAndServeTLS(certFile, keyFile)
	} else {
		fmt.Printf("Mamla: http://%s\n", addr)
		err = server.ListenAndServe()
	}
	if !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}
