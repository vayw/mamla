package main

import (
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"time"
)

const authCookie = "__Host-mamla_auth"
const authLifetime = 7 * 24 * time.Hour

var keyPattern = regexp.MustCompile(`^mk_[A-Za-z0-9_-]{43}$`)
var invitePattern = regexp.MustCompile(`^mi_[A-Za-z0-9_-]{43}$`)
var errAccess = errors.New("ключ или приглашение недействительны")

type accessUser struct {
	ID      string `json:"id"`
	Name    string `json:"name"`
	KeyHash string `json:"key_hash"`
	Revoked bool   `json:"revoked"`
}
type accessInvite struct {
	ID      string    `json:"id"`
	Name    string    `json:"name"`
	Hash    string    `json:"hash"`
	Expires time.Time `json:"expires"`
	Used    bool      `json:"used"`
}
type accessSession struct {
	Hash    string    `json:"hash"`
	UserID  string    `json:"user_id"`
	Expires time.Time `json:"expires"`
}
type accessData struct {
	Version  int             `json:"version"`
	Users    []accessUser    `json:"users"`
	Invites  []accessInvite  `json:"invites"`
	Sessions []accessSession `json:"sessions"`
}
type authStore struct {
	path string
	mu   sync.Mutex
}
type accessPrincipal struct {
	User        accessUser
	SessionHash string
}
type principalContextKey struct{}

func digest(kind, secret string) string {
	sum := sha256.Sum256([]byte(kind + ":" + secret))
	return hex.EncodeToString(sum[:])
}
func sameHash(a, b string) bool { return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1 }

// The separate lock survives atomic replacement of the data file. Both CLI
// and server use it, so invitation redemption is atomic across processes.
func (s *authStore) data(write bool, fn func(*accessData) error) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := os.MkdirAll(filepath.Dir(s.path), 0700); err != nil {
		return err
	}
	lock, err := os.OpenFile(s.path+".lock", os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err := syscall.Flock(int(lock.Fd()), syscall.LOCK_EX); err != nil {
		return err
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	db := accessData{Version: 1}
	file, err := os.Open(s.path)
	if err == nil {
		defer file.Close()
		info, e := file.Stat()
		if e != nil {
			return e
		}
		if !info.Mode().IsRegular() || info.Mode().Perm()&0077 != 0 {
			return errors.New("AUTH_FILE must be a regular private file (chmod 600)")
		}
		if info.Size() > 16<<20 {
			return errors.New("AUTH_FILE is too large")
		}
		db = accessData{}
		decoder := json.NewDecoder(file)
		if err := decoder.Decode(&db); err != nil {
			return errors.New("invalid AUTH_FILE JSON")
		}
		if err := decoder.Decode(new(any)); err != io.EOF {
			return errors.New("invalid AUTH_FILE JSON suffix")
		}
		if db.Version != 1 {
			return errors.New("unsupported AUTH_FILE version")
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	// Expired browser sessions need not accumulate forever.
	active := db.Sessions[:0]
	for _, session := range db.Sessions {
		if session.Expires.After(time.Now()) {
			active = append(active, session)
		}
	}
	db.Sessions = active
	if err := fn(&db); err != nil {
		return err
	}
	if !write {
		return nil
	}
	tmp, err := os.CreateTemp(filepath.Dir(s.path), ".mamla-auth-*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	defer tmp.Close()
	if err := json.NewEncoder(tmp).Encode(db); err != nil {
		return err
	}
	if err := tmp.Sync(); err != nil {
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), s.path)
}

func (s *authStore) invite(name string, lifetime time.Duration) (string, error) {
	name = strings.TrimSpace(name)
	if name == "" || len([]rune(name)) > 64 || lifetime <= 0 || lifetime > 30*24*time.Hour {
		return "", errors.New("name must be 1–64 characters; expiry must be >0 and <=720h")
	}
	secret := "mi_" + randomID(32)
	err := s.data(true, func(db *accessData) error {
		if len(db.Invites) >= 10000 {
			return errors.New("too many invitations")
		}
		for _, user := range db.Users {
			if user.Name == name && !user.Revoked {
				return errors.New("active access already exists for this name")
			}
		}
		for _, invite := range db.Invites {
			if invite.Name == name && !invite.Used && invite.Expires.After(time.Now()) {
				return errors.New("unused invitation already exists for this name")
			}
		}
		db.Invites = append(db.Invites, accessInvite{ID: randomID(9), Name: name, Hash: digest("invite", secret), Expires: time.Now().Add(lifetime)})
		return nil
	})
	return secret, err
}

func newAccessSession(db *accessData, user accessUser, replaceHash string) (string, error) {
	if replaceHash != "" {
		active := db.Sessions[:0]
		for _, session := range db.Sessions {
			if !sameHash(session.Hash, replaceHash) {
				active = append(active, session)
			}
		}
		db.Sessions = active
	}
	if len(db.Sessions) >= 4096 {
		return "", errors.New("session limit reached")
	}
	secret := "ms_" + randomID(32)
	db.Sessions = append(db.Sessions, accessSession{Hash: digest("session", secret), UserID: user.ID, Expires: time.Now().Add(authLifetime)})
	return secret, nil
}

func (s *authStore) accept(invite, key, replaceHash string) (string, error) {
	if !keyPattern.MatchString(key) || !invitePattern.MatchString(invite) {
		return "", errAccess
	}
	keyHash, inviteHash := digest("key", key), digest("invite", invite)
	var secret string
	err := s.data(true, func(db *accessData) error {
		for _, user := range db.Users {
			if sameHash(user.KeyHash, keyHash) {
				return errAccess
			}
		}
		for i, ticket := range db.Invites {
			if sameHash(ticket.Hash, inviteHash) && !ticket.Used && ticket.Expires.After(time.Now()) {
				if len(db.Users) >= 10000 {
					return errors.New("user limit reached")
				}
				user := accessUser{ID: randomID(12), Name: ticket.Name, KeyHash: keyHash}
				var err error
				secret, err = newAccessSession(db, user, replaceHash)
				if err != nil {
					return err
				}
				db.Invites[i].Used = true
				db.Users = append(db.Users, user)
				return nil
			}
		}
		return errAccess
	})
	return secret, err
}

func (s *authStore) login(key, replaceHash string) (string, error) {
	if !keyPattern.MatchString(key) {
		return "", errAccess
	}
	keyHash := digest("key", key)
	var secret string
	err := s.data(true, func(db *accessData) error {
		for _, user := range db.Users {
			if !user.Revoked && sameHash(user.KeyHash, keyHash) {
				var err error
				secret, err = newAccessSession(db, user, replaceHash)
				return err
			}
		}
		return errAccess
	})
	return secret, err
}

func (s *authStore) principal(r *http.Request) (accessPrincipal, error) {
	var result accessPrincipal
	key := r.Header.Get("X-Mamla-Key")
	if key != "" && !keyPattern.MatchString(key) {
		return result, errAccess
	}
	keyHash := digest("key", key)
	var sessionHash string
	if cookie, err := r.Cookie(authCookie); err == nil {
		sessionHash = digest("session", cookie.Value)
	}
	err := s.data(false, func(db *accessData) error {
		var userID string
		if key == "" {
			if sessionHash == "" {
				return errAccess
			}
			for _, session := range db.Sessions {
				if session.Expires.After(time.Now()) && sameHash(session.Hash, sessionHash) {
					userID = session.UserID
					break
				}
			}
			if userID == "" {
				return errAccess
			}
		}
		for _, user := range db.Users {
			if user.Revoked {
				continue
			}
			if key != "" && sameHash(user.KeyHash, keyHash) {
				result.User = user
				return nil
			}
			if key == "" && user.ID == userID {
				result = accessPrincipal{User: user, SessionHash: sessionHash}
				return nil
			}
		}
		return errAccess
	})
	return result, err
}

func (s *authStore) revoke(name string) error {
	return s.data(true, func(db *accessData) error {
		found := false
		for i, user := range db.Users {
			if user.Name == name || user.ID == name {
				db.Users[i].Revoked = true
				found = true
			}
		}
		for i, invite := range db.Invites {
			if invite.Name == name || invite.ID == name {
				db.Invites[i].Used = true
				found = true
			}
		}
		if !found {
			return errors.New("name or ID not found")
		}
		active := db.Sessions[:0]
		for _, session := range db.Sessions {
			for _, user := range db.Users {
				if user.ID == session.UserID && !user.Revoked {
					active = append(active, session)
					break
				}
			}
		}
		db.Sessions = active
		return nil
	})
}

func setAccessCookie(w http.ResponseWriter, token string) {
	cookie := &http.Cookie{Name: authCookie, Value: token, Path: "/", Secure: true, HttpOnly: true, SameSite: http.SameSiteLaxMode, MaxAge: int(authLifetime.Seconds())}
	if token == "" {
		cookie.MaxAge = -1
	}
	http.SetCookie(w, cookie)
}

func (v *voiceServer) authRoutes(mux *http.ServeMux) {
	mux.HandleFunc("GET /api/auth/status", func(w http.ResponseWriter, r *http.Request) {
		if v.auth == nil {
			respond(w, 200, map[string]any{"enabled": false, "authenticated": true})
			return
		}
		p, err := v.auth.principal(r)
		respond(w, 200, map[string]any{"enabled": true, "authenticated": err == nil, "name": p.User.Name})
	})
	for _, action := range []string{"login", "accept", "logout"} {
		mux.HandleFunc("POST /api/auth/"+action, func(w http.ResponseWriter, r *http.Request) {
			if v.auth == nil {
				fail(w, 404, "Авторизация не включена")
				return
			}
			if !strings.HasPrefix(r.Header.Get("Content-Type"), "application/json") {
				fail(w, 415, "Ожидается application/json")
				return
			}
			var input struct {
				Key    string `json:"key"`
				Invite string `json:"invite"`
			}
			if !decode(w, r, &input) {
				return
			}
			if action == "logout" {
				// Logout is scoped to the browser cookie, not a permanent native key.
				cookieRequest := r.Clone(r.Context())
				cookieRequest.Header.Del("X-Mamla-Key")
				p, err := v.auth.principal(cookieRequest)
				if err != nil && !errors.Is(err, errAccess) {
					fail(w, 503, "Не удалось завершить сессию")
					return
				}
				if err == nil && p.SessionHash != "" {
					err = v.auth.data(true, func(db *accessData) error {
						active := db.Sessions[:0]
						for _, s := range db.Sessions {
							if !sameHash(s.Hash, p.SessionHash) {
								active = append(active, s)
							}
						}
						db.Sessions = active
						return nil
					})
					if err != nil {
						fail(w, 503, "Не удалось завершить сессию")
						return
					}
				}
				setAccessCookie(w, "")
				w.WriteHeader(204)
				return
			}
			var token string
			var err error
			var previous string
			if cookie, err := r.Cookie(authCookie); err == nil {
				previous = digest("session", cookie.Value)
			}
			if action == "accept" {
				token, err = v.auth.accept(input.Invite, input.Key, previous)
			} else {
				token, err = v.auth.login(input.Key, previous)
			}
			if err != nil {
				if errors.Is(err, errAccess) {
					fail(w, 401, "Ключ или приглашение недействительны, использованы или просрочены")
				} else {
					fail(w, 503, "Не удалось сохранить доступ")
				}
				return
			}
			setAccessCookie(w, token)
			w.WriteHeader(204)
		})
	}
}

// Returns handled=true for administrative commands. Secrets are printed only
// for newly created invitation URLs, never for personal keys or stored hashes.
func authCLI(args []string, out io.Writer) (bool, error) {
	if len(args) == 0 {
		return false, nil
	}
	path := os.Getenv("AUTH_FILE")
	if path == "" {
		return true, errors.New("set AUTH_FILE to the same private file used by the server")
	}
	store := &authStore{path: path}
	if len(args) >= 3 && args[0] == "invite" && args[1] == "create" {
		flags := flag.NewFlagSet("invite create NAME", flag.ContinueOnError)
		flags.SetOutput(io.Discard)
		expires := flags.Duration("expires", 24*time.Hour, "invitation lifetime")
		if err := flags.Parse(args[3:]); err != nil {
			return true, err
		}
		if flags.NArg() != 0 {
			return true, errors.New("unexpected arguments")
		}
		base, err := url.Parse(os.Getenv("PUBLIC_URL"))
		if err != nil || base.Scheme != "https" || base.Host == "" || base.User != nil || base.RawQuery != "" || base.Fragment != "" || (base.Path != "" && base.Path != "/") {
			return true, errors.New("set PUBLIC_URL to the public HTTPS origin, e.g. https://mamla.io")
		}
		secret, err := store.invite(args[2], *expires)
		if err != nil {
			return true, err
		}
		base.Path = "/"
		base.Fragment = "invite=" + secret
		_, err = fmt.Fprintln(out, base.String())
		return true, err
	}
	if len(args) == 2 && args[0] == "access" && args[1] == "list" {
		return true, store.data(false, func(db *accessData) error {
			for _, u := range db.Users {
				fmt.Fprintf(out, "user\t%s\t%s\trevoked=%t\n", u.ID, u.Name, u.Revoked)
			}
			for _, i := range db.Invites {
				fmt.Fprintf(out, "invite\t%s\t%s\tused=%t\texpires=%s\n", i.ID, i.Name, i.Used, i.Expires.UTC().Format(time.RFC3339))
			}
			return nil
		})
	}
	if len(args) == 3 && args[0] == "access" && args[1] == "revoke" {
		return true, store.revoke(args[2])
	}
	return true, errors.New("usage: invite create NAME [--expires 24h] | access list | access revoke NAME_OR_ID")
}

func withPrincipal(r *http.Request, principal accessPrincipal) *http.Request {
	return r.WithContext(context.WithValue(r.Context(), principalContextKey{}, principal))
}
