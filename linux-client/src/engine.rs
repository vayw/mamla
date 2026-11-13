use crate::{
    media::{self, AudioOptions, MediaEvent, Peer},
    protocol::{self, Api, Config, Connect, Event, Participant, Session},
};
use anyhow::{bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::mpsc, time::Duration};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub enum Command {
    Mute(bool),
    Leave,
}

pub struct Row {
    pub name: String,
    pub status: String,
}

pub enum UiEvent {
    Connected,
    Participants(Vec<Row>),
    Muted(bool),
    Stopped(Option<String>),
}

struct Remote {
    person: Participant,
    media: Option<Peer>,
    initiator: bool,
    offered: bool,
    remote_set: bool,
    ice: Vec<Value>,
    status: String,
}

fn publish(
    ui: &mpsc::Sender<UiEvent>,
    options: &Connect,
    muted: bool,
    peers: &BTreeMap<String, Remote>,
) {
    let mut rows = vec![Row {
        name: format!("{} · вы", options.name),
        status: if muted {
            "Микрофон выключен"
        } else {
            "Микрофон включён"
        }
        .into(),
    }];
    rows.extend(peers.values().map(|p| Row {
        name: p.person.name.clone(),
        status: if p.person.muted {
            "Микрофон выключен".into()
        } else {
            p.status.clone()
        },
    }));
    let _ = ui.send(UiEvent::Participants(rows));
}

fn remote(person: Participant, initiator: bool) -> Remote {
    Remote {
        person,
        media: None,
        initiator,
        offered: false,
        remote_set: false,
        ice: vec![],
        status: "Подключение…".into(),
    }
}

pub fn start(
    mut options: Connect,
    ui: mpsc::Sender<UiEvent>,
    gate: std::sync::Arc<crate::gate::Settings>,
) -> UnboundedSender<Command> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            options.validate()?;
            media::check_plugins()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let audio = AudioOptions {
                input: options.input.clone(),
                output: options.output.clone(),
                synthetic: false,
                gate,
            };
            runtime.block_on(connect(options, rx, &ui, audio))
        })();
        let _ = ui.send(UiEvent::Stopped(
            result.err().map(|error| error.to_string()),
        ));
    });
    tx
}

async fn connect(
    mut options: Connect,
    commands: UnboundedReceiver<Command>,
    ui: &mpsc::Sender<UiEvent>,
    audio: AudioOptions,
) -> Result<()> {
    if options.name.is_empty() {
        options.name = "Гость".into();
    }
    let api = Api::new(&options)?;
    let config: Config = protocol::check(
        api.client
            .get(format!("{}/api/config", api.base))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("Не удалось получить конфигурацию сервера. Проверьте адрес и сертификат")?,
    )
    .await?
    .json()
    .await?;
    let session: Session = api
        .post(
            "join",
            "",
            &json!({"room":options.room, "name":options.name}),
        )
        .await?
        .json()
        .await?;
    if !session.name.is_empty() {
        options.name = session.name.clone();
    }
    // Always release the server session, even if opening SSE or audio setup fails.
    let result = run_session(&api, &session, &options, &config, commands, ui, &audio).await;
    let _ = api.post("leave", &session.token, &Value::Null).await;
    result
}

async fn send_signal(
    api: &Api,
    session: &Session,
    id: &str,
    kind: &str,
    data: Value,
) -> Result<()> {
    let response = api
        .client
        .post(format!("{}/api/signal", api.base))
        .bearer_auth(&session.token)
        .json(&json!({"to":id, "type":kind, "data":data}))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("Потеряна связь с сервером сигналинга")?;
    // A leave can overtake an in-flight ICE candidate; it should not end the room.
    if response.status() != reqwest::StatusCode::NOT_FOUND {
        protocol::check(response).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Go server; set MAMLA_TEST_SERVER=http://127.0.0.1:8080"]
    fn three_clients_join_mute_and_leave() {
        media::check_plugins().unwrap();
        let server = std::env::var("MAMLA_TEST_SERVER").expect("set MAMLA_TEST_SERVER");
        let room = format!(
            "rust-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut commands = vec![];
            let mut updates = vec![];
            let mut tasks = vec![];
            for index in 0..3 {
                let options = Connect {
                    server: server.clone(),
                    room: room.clone(),
                    name: format!("Client {index}"),
                    access_key: std::env::var("MAMLA_TEST_KEY").unwrap_or_default(),
                    certificate: None,
                    insecure_tls: false,
                    input: String::new(),
                    output: String::new(),
                };
                let (cmd_tx, cmd_rx) = unbounded_channel();
                let (ui_tx, ui_rx) = mpsc::channel();
                commands.push(cmd_tx);
                updates.push(ui_rx);
                tasks.push(tokio::spawn(async move {
                    connect(
                        options,
                        cmd_rx,
                        &ui_tx,
                        AudioOptions {
                            synthetic: true,
                            ..Default::default()
                        },
                    )
                    .await
                }));
            }
            let mut states: [Vec<Row>; 3] = Default::default();
            async fn wait_for(
                updates: &[mpsc::Receiver<UiEvent>],
                states: &mut [Vec<Row>; 3],
                predicate: impl Fn(&[Vec<Row>; 3]) -> bool,
            ) {
                tokio::time::timeout(Duration::from_secs(25), async {
                    loop {
                        for (index, rx) in updates.iter().enumerate() {
                            for event in rx.try_iter() {
                                if let UiEvent::Participants(rows) = event {
                                    states[index] = rows;
                                }
                            }
                        }
                        if predicate(states) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("group state did not converge");
            }
            wait_for(&updates, &mut states, |s| {
                s.iter().all(|rows| {
                    rows.len() == 3 && rows.iter().filter(|r| r.status == "На связи").count() == 2
                })
            })
            .await;
            commands[0].send(Command::Mute(true)).unwrap();
            wait_for(&updates, &mut states, |s| {
                s.iter()
                    .all(|rows| rows.iter().any(|r| r.status == "Микрофон выключен"))
            })
            .await;
            commands[0].send(Command::Leave).unwrap();
            wait_for(&updates, &mut states, |s| {
                s[1..].iter().all(|rows| rows.len() == 2)
            })
            .await;
            for tx in &commands[1..] {
                tx.send(Command::Leave).unwrap();
            }
            for task in tasks {
                tokio::time::timeout(Duration::from_secs(12), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
        });
    }
}

async fn run_session(
    api: &Api,
    session: &Session,
    options: &Connect,
    config: &Config,
    mut commands: UnboundedReceiver<Command>,
    ui: &mpsc::Sender<UiEvent>,
    audio: &AudioOptions,
) -> Result<()> {
    let response = protocol::check(
        api.client
            .get(format!("{}/api/events", api.base))
            .bearer_auth(&session.token)
            .send()
            .await
            .context("Не удалось открыть поток сигналинга")?,
    )
    .await?;
    let mut events = response.bytes_stream().eventsource();
    let (media_tx, mut media_rx) = unbounded_channel();
    let mut peers = BTreeMap::<String, Remote>::new();
    let mut muted = false;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Mute(value)) => {
                    muted = value;
                    for peer in peers.values() { if let Some(media) = &peer.media { media.mute(muted); } }
                    let _ = ui.send(UiEvent::Muted(muted));
                    publish(ui, options, muted, &peers);
                    api.post("mute", &session.token, &json!({"muted":muted})).await?;
                }
                Some(Command::Leave) | None => break,
            },
            incoming = events.next() => {
                let frame = incoming.context("Соединение с сервером закрыто. Войдите заново")?.context("Ошибка потока сигналинга")?;
                let event: Event = serde_json::from_str(&frame.data).context("Некорректное событие сервера")?;
                match event {
                    Event::Ready { peers: people } => {
                        let _ = ui.send(UiEvent::Connected);
                        for person in people {
                            if person.id == session.id || peers.contains_key(&person.id) { continue; }
                            let mut p = remote(person, true);
                            p.media = Some(Peer::new(&p.person.id, config, &audio, 96, muted, media_tx.clone())?);
                            peers.insert(p.person.id.clone(), p);
                        }
                    }
                    Event::Joined(person) => { if person.id != session.id { peers.entry(person.id.clone()).or_insert_with(|| remote(person, false)); } }
                    Event::Updated(person) => { if let Some(peer) = peers.get_mut(&person.id) { peer.person = person; } }
                    Event::Left { id } => { peers.remove(&id); }
                    Event::Signal { from, kind, data } => {
                        if let Some(peer) = peers.get_mut(&from) {
                            match kind.as_str() {
                                "ice" => {
                                    if peer.remote_set { if let Some(media) = &peer.media { media.ice(&data)?; } }
                                    else {
                                        if peer.ice.len() >= 256 { bail!("Слишком много ICE-кандидатов"); }
                                        peer.ice.push(data);
                                    }
                                }
                                "offer" | "answer" => {
                                    if data.get("type").and_then(Value::as_str) != Some(kind.as_str()) { bail!("Несогласованный тип SDP"); }
                                    let sdp = data.get("sdp").and_then(Value::as_str).context("Пустой SDP участника")?;
                                    if peer.media.is_none() {
                                        if kind != "offer" { bail!("Ответ без предложения SDP"); }
                                        let payload = media::opus_payload(sdp)?;
                                        peer.media = Some(Peer::new(&from, config, &audio, payload, muted, media_tx.clone())?);
                                    }
                                    peer.media.as_ref().unwrap().remote_description(&kind, sdp)?;
                                }
                                _ => (),
                            }
                        }
                    }
                }
                publish(ui, options, muted, &peers);
            },
            event = media_rx.recv() => {
                let Some(event) = event else { break; };
                match event {
                    MediaEvent::Negotiate(id) => {
                        if let Some(peer) = peers.get_mut(&id) {
                            if peer.initiator && !peer.offered {
                                peer.offered = true;
                                if let Some(media) = &peer.media { media.create_description(false); }
                            }
                        }
                    }
                    MediaEvent::LocalDescription { id, kind, sdp } => {
                        if peers.contains_key(&id) { send_signal(api, session, &id, &kind, json!({"type":kind, "sdp":sdp})).await?; }
                    }
                    MediaEvent::Ice { id, data } => {
                        if peers.contains_key(&id) { send_signal(api, session, &id, "ice", data).await?; }
                    }
                    MediaEvent::RemoteSet { id, answer } => {
                        if let Some(peer) = peers.get_mut(&id) {
                            peer.remote_set = true;
                            if let Some(media) = &peer.media {
                                for candidate in peer.ice.drain(..) { media.ice(&candidate)?; }
                                if answer { media.create_description(true); }
                            }
                        }
                    }
                    MediaEvent::State { id, state } => { if let Some(peer) = peers.get_mut(&id) { peer.status = state; publish(ui, options, muted, &peers); } }
                    MediaEvent::Failed { id, message } => { if peers.contains_key(&id) { bail!("{message}"); } }
                    #[cfg(test)]
                    MediaEvent::Audio(_) => (),
                }
            }
        }
    }
    Ok(())
}
