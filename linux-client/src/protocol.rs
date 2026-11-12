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
    pub certificate: Option<PathBuf>,
    pub input: String,
    pub output: String,
}

impl Connect {
    pub fn validate(&mut self) -> Result<()> {
        self.name = self.name.trim().to_owned();
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
        if self.name.is_empty() || self.name.chars().count() > 32 {
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
            .connect_timeout(Duration::from_secs(8))
            .read_timeout(Duration::from_secs(40));
        if let Some(path) = &options.certificate {
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
            certificate: None,
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
    }

    #[test]
    fn decodes_browser_signal_and_ice_urls() {
        let event: Event = serde_json::from_str(r#"{"type":"signal","data":{"from":"peer","type":"ice","data":{"candidate":"candidate:...","sdpMLineIndex":0}}}"#).unwrap();
        assert!(matches!(event, Event::Signal { kind, .. } if kind == "ice"));
        let config: Config = serde_json::from_str(r#"{"iceServers":[{"urls":"stun:one:3478"},{"urls":["turn:two:3478","turns:two:5349"],"username":"u","credential":"p"}]}"#).unwrap();
        assert_eq!(config.ice_servers[0].urls(), vec!["stun:one:3478"]);
        assert_eq!(config.ice_servers[1].urls().len(), 2);
    }
}
