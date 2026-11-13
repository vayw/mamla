use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use url::Url;

#[derive(Clone)]
pub struct Connect {
    pub server: String,
    pub room: String,
    pub name: String,
    pub access_key: String,
    pub certificate: Option<PathBuf>,
    pub insecure_tls: bool,
    pub input: String,
    pub output: String,
}

impl Connect {
    pub fn validate(&mut self) -> Result<()> {
        self.name = self.name.trim().to_owned();
        self.access_key = self.access_key.trim().to_owned();
        if !self.access_key.is_empty()
            && (!self.access_key.starts_with("mk_")
                || self.access_key.len() != 46
                || !self.access_key[3..]
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
        {
            bail!("Нужен личный ключ доступа mk_… из приглашения");
        }
        self.room = self.room.trim().to_owned();
        let url = Url::parse(self.server.trim())
            .context("Нужен URL сервера: http://host:port или https://host:port")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!("Укажите только адрес сервера, без комнаты, пароля и пути");
        }
        if self.insecure_tls && url.scheme() != "https" {
            bail!("Вход без проверки сертификата доступен только для HTTPS");
        }
        self.server = url.as_str().trim_end_matches('/').to_owned();
        let valid_room = !self.room.is_empty()
            && self.room.len() <= 64
            && self.room.as_bytes()[0].is_ascii_alphanumeric()
            && self
                .room
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if !valid_room {
            bail!("ID комнаты: 1–64 символа a–z, 0–9 и дефис; первый символ — буква или цифра");
        }
        if self.access_key.is_empty() && (self.name.is_empty() || self.name.chars().count() > 32) {
            bail!("Имя должно содержать от 1 до 32 символов");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Participant {
    pub id: String,
    pub name: String,
    pub muted: bool,
}

#[derive(Deserialize)]
pub struct Session {
    pub id: String,
    pub token: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub ice_servers: Vec<IceServer>,
}

#[derive(Clone, Deserialize)]
pub struct IceServer {
    pub urls: Value,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

impl IceServer {
    pub fn urls(&self) -> Vec<&str> {
        match &self.urls {
            Value::String(url) => vec![url.as_str()],
            Value::Array(urls) => urls.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    #[serde(rename = "ready")]
    Ready { peers: Vec<Participant> },
    #[serde(rename = "peer-joined")]
    Joined(Participant),
    #[serde(rename = "peer-updated")]
    Updated(Participant),
    #[serde(rename = "peer-left")]
    Left { id: String },
    #[serde(rename = "signal")]
    Signal {
        from: String,
        #[serde(rename = "type")]
        kind: String,
        data: Value,
    },
}

pub struct Api {
    pub client: reqwest::Client,
    pub base: String,
}

impl Api {
    pub fn new(options: &Connect) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(8))
            .read_timeout(Duration::from_secs(40));
        if !options.access_key.is_empty() {
            let mut headers = reqwest::header::HeaderMap::new();
            let mut key = reqwest::header::HeaderValue::from_bytes(options.access_key.as_bytes())
                .context("Некорректный ключ доступа")?;
            key.set_sensitive(true);
            headers.insert("x-mamla-key", key);
            builder = builder.default_headers(headers);
        }
        if options.insecure_tls {
            // Explicit opt-in for this connection only. Never follow a redirect
            // with this verifier, or the bypass would apply to another server.
            builder = builder
                .danger_accept_invalid_certs(true)
                .redirect(reqwest::redirect::Policy::none());
        } else if let Some(path) = &options.certificate {
            let pem = std::fs::read(path).context("Не удалось прочитать PEM-сертификат сервера")?;
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&pem).context("Некорректный PEM-сертификат")?,
            );
        }
        Ok(Self {
            client: builder.build()?,
            base: options.server.clone(),
        })
    }

    pub async fn post(&self, path: &str, token: &str, data: &Value) -> Result<reqwest::Response> {
        let mut request = self
            .client
            .post(format!("{}/api/{path}", self.base))
            .json(data)
            .timeout(Duration::from_secs(10));
        if !token.is_empty() {
            request = request.bearer_auth(token);
        }
        check(
            request
                .send()
                .await
                .context("Нет связи с сервером. Проверьте адрес, сеть и доверие сертификату")?,
        )
        .await
    }
}

pub async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    if status.as_u16() == 401 && response.headers().contains_key("www-authenticate") {
        bail!("Нужен личный ключ доступа в сервис; ключ отсутствует, недействителен или отозван");
    }
    // Do not log arbitrary response bodies, SDP, tokens or TURN credentials.
    match status.as_u16() {
        401 => bail!("Сессия завершена — войдите заново"),
        409 => bail!("Комната заполнена или сессия уже подключена"),
        404 => bail!("Участник отключился"),
        _ => bail!("Сервер ответил HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(server: &str, room: &str) -> Connect {
        Connect {
            server: server.into(),
            room: room.into(),
            name: "  Тест  ".into(),
            access_key: String::new(),
            certificate: None,
            insecure_tls: false,
            input: String::new(),
            output: String::new(),
        }
    }

    #[test]
    fn validates_origin_room_and_unicode_name() {
        let mut o = options("https://192.168.0.235:8443", "local-test");
        o.validate().unwrap();
        assert_eq!(o.name, "Тест");
        for server in [
            "ftp://localhost",
            "http://u:p@localhost",
            "http://localhost/path",
            "http://localhost/?room=x",
        ] {
            assert!(options(server, "room").validate().is_err());
        }
        for room in ["", "-room", "../room", "UPPER", "привет"] {
            assert!(options("http://localhost:8080", room).validate().is_err());
        }
        let mut keyed = options("https://mamla.example", "room");
        keyed.access_key = format!("mk_{}", "A".repeat(43));
        keyed.name.clear();
        keyed.validate().unwrap();
        keyed.access_key.push('\n');
        keyed.access_key.push_str("Injected: header");
        assert!(keyed.validate().is_err());
    }

    #[test]
    fn decodes_browser_signal_and_ice_urls() {
        let event: Event = serde_json::from_str(r#"{"type":"signal","data":{"from":"peer","type":"ice","data":{"candidate":"candidate:...","sdpMLineIndex":0}}}"#).unwrap();
        assert!(matches!(event, Event::Signal { kind, .. } if kind == "ice"));
        let config: Config = serde_json::from_str(r#"{"iceServers":[{"urls":"stun:one:3478"},{"urls":["turn:two:3478","turns:two:5349"],"username":"u","credential":"p"}]}"#).unwrap();
        assert_eq!(config.ice_servers[0].urls(), vec!["stun:one:3478"]);
        assert_eq!(config.ice_servers[1].urls().len(), 2);
    }

    #[test]
    fn tls_bypass_is_explicit_temporary_and_does_not_follow_redirects() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            },
        };

        struct Server {
            url: String,
            certificate: PathBuf,
            stop: Arc<AtomicBool>,
            thread: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for Server {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Relaxed);
                if let Some(thread) = self.thread.take() {
                    thread.join().unwrap();
                }
                let _ = std::fs::remove_file(&self.certificate);
            }
        }

        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![key.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.key_pair.serialize_der()).into(),
        )
        .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let certificate =
            std::env::temp_dir().join(format!("mamla-test-cert-{}-{port}.pem", std::process::id()));
        std::fs::write(&certificate, key.cert.pem()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                        let mut tls = rustls::StreamOwned::new(connection, socket);
                        let mut request = [0; 4096];
                        if let Ok(length) = tls.read(&mut request) {
                            if length == 0 {
                                continue;
                            }
                            let response = if request[..length].starts_with(b"GET /redirect ") {
                                "HTTP/1.1 302 Found\r\nLocation: https://must-not-follow.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            } else {
                                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK"
                            };
                            let _ = tls.write_all(response.as_bytes());
                            let _ = tls.flush();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("TLS fixture: {error}"),
                }
            }
        });
        let server = Server {
            url: format!("https://localhost:{port}"),
            certificate,
            stop,
            thread: Some(thread),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let secure = options(&server.url, "room");
            assert!(!secure.insecure_tls);
            assert!(Api::new(&secure)
                .unwrap()
                .client
                .get(&server.url)
                .send()
                .await
                .unwrap_err()
                .is_connect());

            let mut insecure = secure.clone();
            insecure.insecure_tls = true;
            // A leftover certificate path must not be read in bypass mode.
            insecure.certificate = Some(PathBuf::from("/no-such-mamla-certificate.pem"));
            insecure.validate().unwrap();
            let api = Api::new(&insecure).unwrap();
            assert_eq!(
                api.client.get(&server.url).send().await.unwrap().status(),
                200
            );
            assert_eq!(
                api.client
                    .get(format!("{}/redirect", server.url))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                302
            );

            // Creating another ordinary connection must restore strict verification.
            assert!(Api::new(&secure)
                .unwrap()
                .client
                .get(&server.url)
                .send()
                .await
                .unwrap_err()
                .is_connect());
            let mut trusted = secure;
            trusted.certificate = Some(server.certificate.clone());
            assert_eq!(
                Api::new(&trusted)
                    .unwrap()
                    .client
                    .get(&server.url)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );

            insecure.server = "http://localhost:8080".into();
            assert!(insecure.validate().is_err());
        });
    }
}
