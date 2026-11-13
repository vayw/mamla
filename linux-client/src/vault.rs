//! Secret Service adapter. Keys go through stdin/stdout, never command arguments.
use anyhow::{bail, Context, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
};

pub fn valid_key(key: &str) -> bool {
    key.len() == 46
        && key.starts_with("mk_")
        && key[3..]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
pub fn load(server: &str) -> Result<Option<String>> {
    let server = crate::profiles::origin(server)?;
    let output = Command::new("secret-tool")
        .args(["lookup", "application", "mamla", "server", &server])
        .stdin(Stdio::null())
        .output()
        .context("Установите secret-tool и запустите Secret Service")?;
    if !output.status.success() {
        if output.stderr.is_empty() {
            return Ok(None);
        }
        bail!("Хранилище ключей недоступно или заблокировано");
    }
    let key = String::from_utf8(output.stdout).context("Некорректный ответ хранилища")?;
    let key = key.trim();
    if !valid_key(key) {
        bail!("В хранилище некорректный ключ Mamla");
    }
    Ok(Some(key.into()))
}
pub fn save(server: &str, key: &str) -> Result<()> {
    let server = crate::profiles::origin(server)?;
    if !valid_key(key) {
        bail!("Некорректный ключ доступа");
    }
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            &format!("--label=Mamla: {server}"),
            "application",
            "mamla",
            "server",
            &server,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Установите secret-tool и запустите Secret Service")?;
    let write = child
        .stdin
        .take()
        .context("Нет stdin хранилища")?
        .write_all(key.as_bytes());
    let result = child.wait_with_output()?;
    write?;
    if !result.status.success() {
        bail!("Не удалось сохранить ключ в системном хранилище");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_are_checked_without_echoing_their_contents() {
        assert!(valid_key(&format!("mk_{}", "A".repeat(43))));
        assert!(!valid_key("mk_bad\nInjected: header"));
        assert!(!valid_key(&"é".repeat(23)));
    }
    #[test]
    #[ignore = "requires an isolated unlocked Secret Service; do not run against a real user keyring"]
    fn isolated_secret_service_roundtrip() {
        let server = format!("https://test-{}.mamla.invalid", std::process::id());
        let key = format!("mk_{}", "A".repeat(43));
        save(&server, &key).unwrap();
        assert!(load(&server).unwrap().as_deref() == Some(key.as_str()));
        assert!(load(&format!("{server}:8443")).unwrap().is_none());
        assert!(load(&server.replacen("https:", "http:", 1))
            .unwrap()
            .is_none());
        let result = Command::new("secret-tool")
            .args(["clear", "application", "mamla", "server", &server])
            .output()
            .unwrap();
        assert!(result.status.success());
    }
}
