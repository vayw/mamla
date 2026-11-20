use crate::protocol::Config;
use anyhow::{anyhow, bail, Context, Result};
use gst::prelude::*;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum MediaEvent {
    Statistics {
        id: String,
        data: Value,
    },
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
    pub gate: Arc<crate::gate::Settings>,
    pub diagnostics: Arc<crate::diagnostics::Diagnostics>,
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
    metrics: Arc<AudioMetrics>,
    diagnostics: Arc<crate::diagnostics::Diagnostics>,
    stats_pending: Arc<AtomicBool>,
    gate: Arc<crate::gate::Settings>,
}

struct AudioMetrics {
    started: Instant,
    capture_buffers: AtomicU64,
    playback_buffers: AtomicU64,
    capture_at: AtomicU64,
    playback_at: AtomicU64,
    input_rms: AtomicU32,
    send_rms: AtomicU32,
}

impl Default for AudioMetrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            capture_buffers: AtomicU64::new(0),
            playback_buffers: AtomicU64::new(0),
            capture_at: AtomicU64::new(0),
            playback_at: AtomicU64::new(0),
            input_rms: AtomicU32::new(0),
            send_rms: AtomicU32::new(0),
        }
    }
}

impl AudioMetrics {
    fn snapshot(&self, muted: bool) -> Value {
        let age = |at: &AtomicU64, count: &AtomicU64| {
            (count.load(Ordering::Relaxed) > 0).then(|| {
                (self.started.elapsed().as_micros() as u64)
                    .saturating_sub(at.load(Ordering::Relaxed))
                    / 1000
            })
        };
        json!({
            "microphone":{"muted":muted,"buffers":self.capture_buffers.load(Ordering::Relaxed),
                "ageMs":age(&self.capture_at, &self.capture_buffers),
                "rms":f32::from_bits(self.input_rms.load(Ordering::Relaxed)),
                "sendRms":f32::from_bits(self.send_rms.load(Ordering::Relaxed))},
            "playback":{"buffers":self.playback_buffers.load(Ordering::Relaxed),
                "ageMs":age(&self.playback_at, &self.playback_buffers)}
        })
    }
}

fn meter(
    pad: &gst::Pad,
    metrics: Arc<AudioMetrics>,
    diagnostics: Arc<crate::diagnostics::Diagnostics>,
    input: bool,
) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if !diagnostics.enabled() {
            return gst::PadProbeReturn::Ok;
        }
        if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() {
            if let Ok(map) = buffer.map_readable() {
                let mut total = 0.0_f64;
                let mut count = 0;
                for bytes in map.as_slice().chunks_exact(4) {
                    let value = f32::from_le_bytes(bytes.try_into().unwrap());
                    if value.is_finite() {
                        total += (value as f64).powi(2);
                        count += 1;
                    }
                }
                let rms = if count > 0 {
                    (total / count as f64).sqrt() as f32
                } else {
                    0.0
                };
                if input {
                    metrics.input_rms.store(rms.to_bits(), Ordering::Relaxed);
                    metrics.capture_buffers.fetch_add(1, Ordering::Relaxed);
                    metrics.capture_at.store(
                        metrics.started.elapsed().as_micros() as u64,
                        Ordering::Relaxed,
                    );
                } else {
                    metrics.send_rms.store(rms.to_bits(), Ordering::Relaxed);
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
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

fn attach_gate(pipeline: &gst::Pipeline, settings: Arc<crate::gate::Settings>) -> Result<()> {
    let processor = Mutex::new(crate::gate::Gate::default());
    let pad = pipeline
        .by_name("noisegate")
        .context("Нет noise gate")?
        .static_pad("src")
        .context("Нет audio pad")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() {
            let result = (|| {
                let mut processor = processor.lock().map_err(|_| ())?;
                let mut data = buffer.make_mut().map_writable().map_err(|_| ())?;
                processor.process(data.as_mut_slice(), &settings);
                Ok::<_, ()>(())
            })();
            // Fail closed rather than transmit unprocessed audio.
            if result.is_err() {
                return gst::PadProbeReturn::Drop;
            }
        }
        gst::PadProbeReturn::Ok
    })
    .context("Не удалось подключить noise gate")?;
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
        let description = format!("webrtcbin name=rtc bundle-policy=max-bundle latency=60 {source} ! audio/x-raw ! audioconvert ! audioresample ! audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=1 ! identity name=noisegate ! volume name=microphone ! audioconvert ! opusenc bitrate=32000 audio-type=voice inband-fec=true dtx=true ! rtpopuspay pt={payload} ! application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload={payload} ! queue ! rtc.");
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("Не удалось создать аудиопайплайн"))?;
        let rtc = pipeline.by_name("rtc").context("Нет webrtcbin")?;
        let volume = pipeline.by_name("microphone").context("Нет volume")?;
        let metrics = Arc::new(AudioMetrics::default());
        meter(
            &pipeline
                .by_name("noisegate")
                .context("Нет noise gate")?
                .static_pad("sink")
                .context("Нет input pad")?,
            metrics.clone(),
            audio.diagnostics.clone(),
            true,
        );
        meter(
            &volume.static_pad("src").context("Нет microphone pad")?,
            metrics.clone(),
            audio.diagnostics.clone(),
            false,
        );
        volume.set_property("mute", muted);
        attach_gate(&pipeline, audio.gate.clone())?;
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
            let diagnostics = audio.diagnostics.clone();
            rtc.connect_notify(Some("connection-state"), move |rtc, _| {
                let connection =
                    rtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
                diagnostics.event(
                    match connection {
                        gst_webrtc::WebRTCPeerConnectionState::Connected => "connection-connected",
                        gst_webrtc::WebRTCPeerConnectionState::Failed => "connection-failed",
                        gst_webrtc::WebRTCPeerConnectionState::Disconnected => {
                            "connection-disconnected"
                        }
                        gst_webrtc::WebRTCPeerConnectionState::Closed => "connection-closed",
                        gst_webrtc::WebRTCPeerConnectionState::Connecting => {
                            "connection-connecting"
                        }
                        _ => "connection-new",
                    },
                    Some(&id),
                );
                let state = match connection {
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
            let metrics = metrics.clone();
            let tx = tx.clone();
            let id = id.to_owned();
            rtc.connect_pad_added(move |_, pad| {
                if pad.direction() != gst::PadDirection::Src { return; }
                let Some(pipeline) = weak.upgrade() else { return; };
                let result = (|| -> Result<()> {
                    let sink = if audio.synthetic { "fakesink name=output sync=false async=false signal-handoffs=true" } else { "pipewiresink name=output" };
                    let bin = gst::parse::bin_from_description(&format!("queue ! rtpopusdepay ! opusdec ! audioconvert ! audioresample ! {sink}"), true)?;
                    let output = bin.by_name("output").context("Нет audio sink")?;
                    let playback_metrics = metrics.clone();
                    let diagnostics = audio.diagnostics.clone();
                    output.static_pad("sink").context("Нет output pad")?.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                        if diagnostics.enabled() {
                            playback_metrics.playback_buffers.fetch_add(1, Ordering::Relaxed);
                            playback_metrics.playback_at.store(playback_metrics.started.elapsed().as_micros() as u64, Ordering::Relaxed);
                        }
                        gst::PadProbeReturn::Ok
                    });
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
            metrics,
            diagnostics: audio.diagnostics.clone(),
            stats_pending: Arc::new(AtomicBool::new(false)),
            gate: audio.gate.clone(),
        };
        peer.pipeline
            .set_state(gst::State::Playing)
            .context("Не удалось запустить PipeWire/WebRTC")?;
        Ok(peer)
    }

    pub fn mute(&self, muted: bool) {
        self.volume.set_property("mute", muted);
    }

    pub fn request_stats(&self) {
        if !self.diagnostics.enabled() || self.stats_pending.swap(true, Ordering::Relaxed) {
            return;
        }
        let pending = self.stats_pending.clone();
        let metrics = self.metrics.clone();
        let tx = self.tx.clone();
        let id = self.id.clone();
        let muted = self.volume.property::<bool>("mute");
        let (gate_enabled, threshold_db) = self.gate.snapshot();
        let connection = format!(
            "{:?}",
            self.rtc
                .property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state")
        )
        .to_ascii_lowercase();
        let ice = format!(
            "{:?}",
            self.rtc
                .property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state")
        )
        .to_ascii_lowercase();
        let signaling = format!(
            "{:?}",
            self.rtc
                .property::<gst_webrtc::WebRTCSignalingState>("signaling-state")
        )
        .to_ascii_lowercase();
        let promise = gst::Promise::with_change_func(move |reply| {
            let mut data = match reply {
                Ok(Some(reply)) if !reply.has_field("error") => {
                    crate::diagnostics::project_stats(reply)
                }
                _ => json!({"statsUnavailable":true}),
            };
            let levels = metrics.snapshot(muted);
            data["microphone"] = levels["microphone"].clone();
            data["microphone"]["gateEnabled"] = json!(gate_enabled);
            data["microphone"]["gateThresholdDb"] = json!(threshold_db);
            data["playback"] = levels["playback"].clone();
            data["connection"] = json!(connection);
            data["ice"] = json!(ice);
            data["signaling"] = json!(signaling);
            pending.store(false, Ordering::Relaxed);
            let _ = tx.send(MediaEvent::Statistics { id, data });
        });
        self.rtc
            .emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
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
    fn gate_processes_real_gstreamer_buffers_without_dropping_them() {
        gst::init().unwrap();
        for (amplitude, quiet) in [(0.001, true), (0.2, false)] {
            let pipeline = gst::parse::launch(&format!("audiotestsrc num-buffers=20 samplesperbuffer=480 volume={amplitude} ! audioconvert ! audio/x-raw,format=F32LE,rate=48000,channels=1 ! identity name=noisegate ! fakesink name=output signal-handoffs=true sync=false"))
                .unwrap().downcast::<gst::Pipeline>().unwrap();
            let settings = Arc::new(crate::gate::Settings::default());
            settings.enable(true);
            attach_gate(&pipeline, settings).unwrap();
            let result = Arc::new(Mutex::new((0_usize, 0_f32)));
            let captured = result.clone();
            pipeline
                .by_name("output")
                .unwrap()
                .connect("handoff", false, move |values| {
                    let buffer = values[1].get::<gst::Buffer>().unwrap();
                    let map = buffer.map_readable().unwrap();
                    let peak = map
                        .as_slice()
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes(c.try_into().unwrap()).abs())
                        .fold(0.0, f32::max);
                    let mut result = captured.lock().unwrap();
                    result.0 += 1;
                    result.1 = result.1.max(peak);
                    None
                });
            pipeline.set_state(gst::State::Playing).unwrap();
            let message = pipeline.bus().unwrap().timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
            pipeline.set_state(gst::State::Null).unwrap();
            assert!(matches!(
                message.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            ));
            let (buffers, peak) = *result.lock().unwrap();
            assert_eq!(buffers, 20);
            if quiet {
                assert_eq!(peak, 0.0);
            } else {
                assert!(peak > 0.19);
            }
        }
    }

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
            audio.diagnostics.set_enabled(true);
            let a = Peer::new("a", &config, &audio, 111, false, tx.clone()).unwrap();
            let b = Peer::new("b", &config, &audio, 111, false, tx).unwrap();
            let mut offered = false;
            let mut ready = [false, false];
            let mut candidates: [Vec<Value>; 2] = Default::default();
            let mut heard = [false, false];
            let mut measured = [false, false];
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    let event = tokio::select! {
                        _ = ticker.tick() => { a.request_stats(); b.request_stats(); continue; },
                        event = rx.recv() => event.expect("media channel closed"),
                    };
                    match event {
                        MediaEvent::Statistics { id, data } => {
                            let sent = data["audio"].as_array().unwrap().iter().any(|row| {
                                row["type"] == "outbound-rtp"
                                    && row["packetsSent"].as_u64().unwrap_or(0) > 0
                            });
                            let received = data["audio"].as_array().unwrap().iter().any(|row| {
                                row["type"] == "inbound-rtp"
                                    && row["packetsReceived"].as_u64().unwrap_or(0) > 0
                            });
                            measured[usize::from(id != "a")] = sent
                                && received
                                && data["route"].is_object()
                                && data["microphone"]["rms"].as_f64().unwrap_or(0.0) > 0.0
                                && data["playback"]["buffers"].as_u64().unwrap_or(0) > 0;
                            audio.diagnostics.sample(&id, data);
                        }
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
                        }
                        MediaEvent::Failed { message, .. } => panic!("{message}"),
                        _ => (),
                    }
                    if heard == [true, true] && measured == [true, true] {
                        break;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "WebRTC loopback audio or diagnostics missing: {}",
                    audio.diagnostics.export()
                )
            });
            assert_eq!(heard, [true, true]);
            assert_eq!(measured, [true, true]);
        });
    }
}
