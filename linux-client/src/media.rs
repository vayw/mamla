use crate::protocol::Config;
use anyhow::{anyhow, bail, Context, Result};
use gst::prelude::*;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum MediaEvent {
    Negotiate(String),
    LocalDescription {
        id: String,
        kind: String,
        sdp: String,
    },
    RemoteSet {
        id: String,
        answer: bool,
    },
    Ice {
        id: String,
        data: Value,
    },
    State {
        id: String,
        state: String,
    },
    Failed {
        id: String,
        message: String,
    },
    #[cfg(test)]
    Audio(String),
}

#[derive(Clone, Default)]
pub struct AudioOptions {
    pub input: String,
    pub output: String,
    pub synthetic: bool,
}

pub fn check_plugins() -> Result<()> {
    gst::init()?;
    let missing: Vec<_> = [
        "webrtcbin",
        "nicesrc",
        "nicesink",
        "dtlssrtpenc",
        "srtpenc",
        "opusenc",
        "opusdec",
        "rtpopuspay",
        "rtpopusdepay",
        "pipewiresrc",
        "pipewiresink",
    ]
    .into_iter()
    .filter(|name| gst::ElementFactory::find(name).is_none())
    .collect();
    if !missing.is_empty() {
        bail!(
            "Не найдены GStreamer-плагины: {}. См. linux-client/README.md",
            missing.join(", ")
        );
    }
    Ok(())
}

pub struct Peer {
    pipeline: gst::Pipeline,
    rtc: gst::Element,
    volume: gst::Element,
    id: String,
    tx: UnboundedSender<MediaEvent>,
}

fn failed(tx: &UnboundedSender<MediaEvent>, id: &str, message: &str) {
    let _ = tx.send(MediaEvent::Failed {
        id: id.into(),
        message: message.into(),
    });
}

fn target(element: &gst::Element, name: &str) -> Result<()> {
    if name.is_empty() {
        return Ok(());
    }
    let property = if element.find_property("target-object").is_some() {
        "target-object"
    } else {
        "path"
    };
    element.set_property(property, name);
    Ok(())
}

// Browser offers usually use PT 111, while our outgoing offers use PT 96.
// Advertise the offered Opus payload before starting the sending pipeline.
pub fn opus_payload(sdp: &str) -> Result<u32> {
    let mut audio = false;
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            audio = line.starts_with("m=audio ");
        }
        if audio {
            if let Some(mapping) = line.strip_prefix("a=rtpmap:") {
                if let Some((pt, codec)) = mapping.split_once(' ') {
                    if codec.to_ascii_lowercase().starts_with("opus/48000") {
                        let payload: u32 = pt.parse()?;
                        if payload <= 127 {
                            return Ok(payload);
                        }
                    }
                }
            }
        }
    }
    bail!("В предложении участника нет аудиокодека Opus");
}

pub fn ice_url(raw: &str, username: &str, password: &str) -> Result<String> {
    let (scheme, rest) = raw
        .split_once(':')
        .ok_or_else(|| anyhow!("Некорректный ICE URL"))?;
    if !matches!(scheme, "stun" | "turn" | "turns") {
        bail!("Неподдерживаемая схема ICE URL");
    }
    let mut url = url::Url::parse(&format!("{scheme}://{}", rest.trim_start_matches('/')))?;
    if scheme != "stun" && !username.is_empty() {
        url.set_username(username)
            .map_err(|_| anyhow!("Некорректный TURN username"))?;
        url.set_password(Some(password))
            .map_err(|_| anyhow!("Некорректный TURN credential"))?;
    }
    Ok(url.to_string())
}

impl Peer {
    pub fn new(
        id: &str,
        config: &Config,
        audio: &AudioOptions,
        payload: u32,
        muted: bool,
        tx: UnboundedSender<MediaEvent>,
    ) -> Result<Self> {
        let source = if audio.synthetic {
            "audiotestsrc is-live=true wave=sine"
        } else {
            "pipewiresrc name=capture"
        };
        let description = format!("webrtcbin name=rtc bundle-policy=max-bundle latency=60 {source} ! audio/x-raw ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 ! volume name=microphone ! opusenc bitrate=32000 inband-fec=true ! rtpopuspay pt={payload} ! application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload={payload} ! queue ! rtc.");
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("Не удалось создать аудиопайплайн"))?;
        let rtc = pipeline.by_name("rtc").context("Нет webrtcbin")?;
        let volume = pipeline.by_name("microphone").context("Нет volume")?;
        volume.set_property("mute", muted);
        if let Some(capture) = pipeline.by_name("capture") {
            target(&capture, &audio.input)?;
        }
        let mut stun_set = false;
        for server in &config.ice_servers {
            for raw in server.urls() {
                let uri = ice_url(raw, &server.username, &server.credential)?;
                if raw.starts_with("stun:") {
                    if !stun_set {
                        rtc.set_property("stun-server", &uri);
                        stun_set = true;
                    }
                } else if !rtc.emit_by_name::<bool>("add-turn-server", &[&uri]) {
                    bail!("GStreamer отклонил настройку TURN");
                }
            }
        }
        {
            let tx = tx.clone();
            let id = id.to_owned();
            rtc.connect("on-negotiation-needed", false, move |_| {
                let _ = tx.send(MediaEvent::Negotiate(id.clone()));
                None
            });
        }
        {
            let tx = tx.clone();
            let id = id.to_owned();
            rtc.connect("on-ice-candidate", false, move |values| {
                if let (Ok(index), Ok(candidate)) =
                    (values[1].get::<u32>(), values[2].get::<String>())
                {
                    let _ = tx.send(MediaEvent::Ice {
                        id: id.clone(),
                        data: json!({"candidate":candidate, "sdpMLineIndex":index}),
                    });
                }
                None
            });
        }
        {
            let tx = tx.clone();
            let id = id.to_owned();
            rtc.connect_notify(Some("connection-state"), move |rtc, _| {
                let state = match rtc
                    .property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state")
                {
                    gst_webrtc::WebRTCPeerConnectionState::Connected => "На связи",
                    gst_webrtc::WebRTCPeerConnectionState::Failed => "Нет связи — перезайдите",
                    gst_webrtc::WebRTCPeerConnectionState::Disconnected => "Связь прервалась",
                    gst_webrtc::WebRTCPeerConnectionState::Closed => "Отключён",
                    _ => "Подключение…",
                };
                let _ = tx.send(MediaEvent::State {
                    id: id.clone(),
                    state: state.into(),
                });
            });
        }
        {
            let weak = pipeline.downgrade();
            let audio = audio.clone();
            let tx = tx.clone();
            let id = id.to_owned();
            rtc.connect_pad_added(move |_, pad| {
                if pad.direction() != gst::PadDirection::Src { return; }
                let Some(pipeline) = weak.upgrade() else { return; };
                let result = (|| -> Result<()> {
                    let sink = if audio.synthetic { "fakesink name=output sync=false async=false signal-handoffs=true" } else { "pipewiresink name=output" };
                    let bin = gst::parse::bin_from_description(&format!("queue ! rtpopusdepay ! opusdec ! audioconvert ! audioresample ! {sink}"), true)?;
                    let output = bin.by_name("output").context("Нет audio sink")?;
                    if !audio.synthetic { target(&output, &audio.output)?; }
                    #[cfg(test)]
                    if audio.synthetic {
                        let tx = tx.clone();
                        let id = id.clone();
                        output.connect("handoff", false, move |_| {
                            let _ = tx.send(MediaEvent::Audio(id.clone()));
                            None
                        });
                    }
                    pipeline.add(&bin)?;
                    let sink = bin.static_pad("sink").context("Нет receive sink pad")?;
                    pad.link(&sink)?;
                    bin.sync_state_with_parent()?;
                    Ok(())
                })();
                if result.is_err() { failed(&tx, &id, "Не удалось подключить воспроизведение аудио"); }
            });
        }
        {
            let tx = tx.clone();
            let id = id.to_owned();
            pipeline.bus().context("Нет audio bus")?.set_sync_handler(move |_, message| {
                match message.view() {
                    gst::MessageView::Error(_) => failed(&tx, &id, "Ошибка аудиопайплайна. Проверьте PipeWire, устройства и GStreamer-плагины"),
                    gst::MessageView::Eos(_) => failed(&tx, &id, "Аудиоустройство отключилось"),
                    _ => (),
                }
                gst::BusSyncReply::Drop
            });
        }
        // Construct the RAII owner before Playing: failures must also release devices.
        let peer = Self {
            pipeline,
            rtc,
            volume,
            id: id.into(),
            tx,
        };
        peer.pipeline
            .set_state(gst::State::Playing)
            .context("Не удалось запустить PipeWire/WebRTC")?;
        Ok(peer)
    }

    pub fn mute(&self, muted: bool) {
        self.volume.set_property("mute", muted);
    }

    pub fn create_description(&self, answer: bool) {
        let kind = if answer { "answer" } else { "offer" };
        let id = self.id.clone();
        let tx = self.tx.clone();
        let rtc = self.rtc.clone();
        let promise = gst::Promise::with_change_func(move |reply| {
            let result = (|| -> Result<()> {
                let reply = reply
                    .map_err(|_| anyhow!("SDP promise failed"))?
                    .context("Empty SDP reply")?;
                let description = reply.get::<gst_webrtc::WebRTCSessionDescription>(kind)?;
                let sdp = description.sdp().as_text()?;
                let ready_tx = tx.clone();
                let ready_id = id.clone();
                let ready = gst::Promise::with_change_func(move |result| {
                    if result.is_ok() {
                        let _ = ready_tx.send(MediaEvent::LocalDescription {
                            id: ready_id,
                            kind: kind.into(),
                            sdp,
                        });
                    } else {
                        failed(&ready_tx, &ready_id, "Не удалось применить локальный SDP");
                    }
                });
                rtc.emit_by_name::<()>("set-local-description", &[&description, &ready]);
                Ok(())
            })();
            if result.is_err() {
                failed(&tx, &id, "Не удалось создать WebRTC SDP");
            }
        });
        self.rtc.emit_by_name::<()>(
            if answer {
                "create-answer"
            } else {
                "create-offer"
            },
            &[&None::<gst::Structure>, &promise],
        );
    }

    pub fn remote_description(&self, kind: &str, sdp: &str) -> Result<()> {
        let sdp_type = match kind {
            "offer" => gst_webrtc::WebRTCSDPType::Offer,
            "answer" => gst_webrtc::WebRTCSDPType::Answer,
            _ => bail!("Неизвестный тип SDP"),
        };
        let parsed = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
            .context("Некорректный SDP участника")?;
        let description = gst_webrtc::WebRTCSessionDescription::new(sdp_type, parsed);
        let tx = self.tx.clone();
        let id = self.id.clone();
        let answer = kind == "offer";
        let promise = gst::Promise::with_change_func(move |reply| {
            let success = match reply {
                Ok(Some(reply)) => !reply.has_field("error"),
                Ok(None) => true,
                Err(_) => false,
            };
            if success {
                let _ = tx.send(MediaEvent::RemoteSet { id, answer });
            } else {
                failed(&tx, &id, "Не удалось применить SDP участника");
            }
        });
        self.rtc
            .emit_by_name::<()>("set-remote-description", &[&description, &promise]);
        Ok(())
    }

    pub fn ice(&self, data: &Value) -> Result<()> {
        let candidate = data
            .get("candidate")
            .and_then(Value::as_str)
            .context("Нет ICE candidate")?;
        let index = data
            .get("sdpMLineIndex")
            .and_then(Value::as_u64)
            .context("Нет ICE media index")?;
        let index = u32::try_from(index)?;
        self.rtc
            .emit_by_name::<()>("add-ice-candidate", &[&index, &candidate]);
        Ok(())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_opus_payload_and_turn_credentials() {
        assert_eq!(opus_payload("m=video 9 UDP/TLS/RTP/SAVPF 96\na=rtpmap:96 VP8/90000\nm=audio 9 UDP/TLS/RTP/SAVPF 111\na=rtpmap:111 opus/48000/2\n").unwrap(), 111);
        assert!(opus_payload("m=audio 9 UDP/TLS/RTP/SAVPF 0\na=rtpmap:0 PCMU/8000").is_err());
        let url = ice_url(
            "turn:relay.example:3478?transport=udp",
            "user@name",
            "p:a@ss",
        )
        .unwrap();
        assert!(url.contains("user%40name:p%3Aa%40ss@"));
    }

    // Exercises real ICE/DTLS/SRTP and Opus encode/decode without touching a microphone.
    #[test]
    fn two_peers_exchange_decoded_audio() {
        gst::init().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let config = Config::default(); // host candidates only, no internet needed
            let audio = AudioOptions {
                synthetic: true,
                ..Default::default()
            };
            let a = Peer::new("a", &config, &audio, 111, false, tx.clone()).unwrap();
            let b = Peer::new("b", &config, &audio, 111, false, tx).unwrap();
            let mut offered = false;
            let mut ready = [false, false];
            let mut candidates: [Vec<Value>; 2] = Default::default();
            let mut heard = [false, false];
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                while let Some(event) = rx.recv().await {
                    match event {
                        MediaEvent::Negotiate(id) if id == "a" && !offered => {
                            offered = true;
                            a.create_description(false);
                        }
                        MediaEvent::LocalDescription { id, kind, sdp } => {
                            let target = if id == "a" { &b } else { &a };
                            target.remote_description(&kind, &sdp).unwrap();
                        }
                        MediaEvent::RemoteSet { id, answer } => {
                            let index = usize::from(id != "a");
                            ready[index] = true;
                            let target = if index == 0 { &a } else { &b };
                            for candidate in candidates[index].drain(..) {
                                target.ice(&candidate).unwrap();
                            }
                            if answer {
                                target.create_description(true);
                            }
                        }
                        MediaEvent::Ice { id, data } => {
                            let target_index = usize::from(id == "a");
                            if ready[target_index] {
                                (if target_index == 0 { &a } else { &b })
                                    .ice(&data)
                                    .unwrap();
                            } else {
                                candidates[target_index].push(data);
                            }
                        }
                        MediaEvent::Audio(id) => {
                            heard[usize::from(id != "a")] = true;
                            if heard == [true, true] {
                                break;
                            }
                        }
                        MediaEvent::Failed { message, .. } => panic!("{message}"),
                        _ => (),
                    }
                }
            })
            .await
            .expect("WebRTC loopback did not deliver audio in both directions");
            assert_eq!(heard, [true, true]);
        });
    }
}
