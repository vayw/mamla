use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

pub fn origin(value: &str) -> Result<String> {
    let url = url::Url::parse(value.trim())?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("Нужен адрес сервера без пути и секретов");
    }
    Ok(url.as_str().trim_end_matches('/').into())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub server: String,
    pub room: String,
}

#[derive(Serialize, Deserialize)]
pub struct Profiles {
    pub entries: Vec<Profile>,
    pub remember_key: bool,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Profiles {
    pub fn load() -> Self {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
        Self::load_from(root.map(|p| p.join("mamla/connections.json")))
    }
    fn load_from(path: Option<PathBuf>) -> Self {
        let mut result = Self {
            entries: vec![],
            remember_key: true,
            path: path.clone(),
        };
        if let Some(path) = &path {
            if let Ok(file) = fs::File::open(path) {
                let mut content = Vec::new();
                let read = file.take(64 * 1024 + 1).read_to_end(&mut content);
                if read.is_ok() && content.len() <= 64 * 1024 {
                    if let Ok(saved) = serde_json::from_slice::<Self>(&content) {
                        result.remember_key = saved.remember_key;
                        for entry in saved.entries.into_iter().take(20) {
                            if let Ok(server) = origin(&entry.server) {
                                if valid_room(&entry.room) {
                                    result.entries.push(Profile {
                                        server,
                                        room: entry.room,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        result
    }
    pub fn remember(&mut self, server: &str, room: &str) -> Result<()> {
        let server = origin(server)?;
        if !valid_room(room) {
            bail!("Некорректная комната");
        }
        self.entries
            .retain(|p| p.server != server || p.room != room);
        self.entries.insert(
            0,
            Profile {
                server,
                room: room.into(),
            },
        );
        self.entries.truncate(20);
        self.save()
    }
    pub fn save(&self) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("Не определён каталог настроек")?;
        let directory = path.parent().context("Нет каталога настроек")?;
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let temporary = directory.join(format!(
            "connections-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn valid_room(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origins_keep_scheme_and_port_separate() {
        assert_eq!(
            origin("HTTPS://MAMLA.EXAMPLE:443/").unwrap(),
            "https://mamla.example"
        );
        assert_ne!(
            origin("http://mamla.example").unwrap(),
            origin("https://mamla.example").unwrap()
        );
        assert_ne!(
            origin("https://mamla.example:8443").unwrap(),
            origin("https://mamla.example").unwrap()
        );
    }
    #[test]
    fn history_is_private_bounded_and_contains_no_credentials() {
        let directory = std::env::temp_dir().join(format!("mamla-profiles-{}", std::process::id()));
        let path = directory.join("connections.json");
        let mut profiles = Profiles::load_from(Some(path.clone()));
        for index in 0..25 {
            profiles
                .remember("https://mamla.example/", &format!("room-{index}"))
                .unwrap();
        }
        profiles
            .remember("https://mamla.example", "room-22")
            .unwrap();
        profiles.remember_key = false;
        profiles.save().unwrap();
        let saved = Profiles::load_from(Some(path.clone()));
        assert!(!saved.remember_key);
        assert_eq!(saved.entries.len(), 20);
        assert_eq!(saved.entries[0].room, "room-22");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(profiles
            .remember("https://user:secret@mamla.example", "room")
            .is_err());
        assert!(profiles
            .remember("https://mamla.example/#invite=secret", "room")
            .is_err());
        assert!(profiles
            .remember("https://mamla.example/?key=secret", "room")
            .is_err());
        assert!(!fs::read_to_string(&path).unwrap().contains("secret"));
        fs::remove_dir_all(directory).unwrap();
    }
}
