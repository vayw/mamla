//! Opt-in, bounded diagnostics. Never store raw SDP, ICE, URLs or error text.
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const LIMIT: usize = 300;

fn numeric(s: &gst::StructureRef, key: &str) -> Option<Value> {
    if let Ok(n) = s.get::<u64>(key) {
        return Some(json!(n));
    }
    if let Ok(n) = s.get::<u32>(key) {
        return Some(json!(n));
    }
    if let Ok(n) = s.get::<i64>(key) {
        return Some(json!(n));
    }
    if let Ok(n) = s.get::<i32>(key) {
        return Some(json!(n));
    }
    s.get::<f64>(key)
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| json!(n))
}

fn candidate(s: &gst::StructureRef) -> Value {
    let kind = s.get::<String>("candidate-type").unwrap_or_default();
    let protocol = s.get::<String>("protocol").unwrap_or_default();
    json!({
        "type":match kind.as_str() { "host"|"srflx"|"prflx"|"relay" => &kind, _ => "unknown" },
        "protocol":match protocol.as_str() { "udp"|"tcp" => &protocol, _ => "unknown" },
    })
}

/// Whitelist projection, never serialize an entire GStreamer stats structure.
pub fn project_stats(reply: &gst::StructureRef) -> Value {
    use gst_webrtc::WebRTCStatsType as T;
    let rows: Vec<(String, gst::Structure)> = reply
        .fields()
        .filter_map(|key| {
            reply
                .get::<gst::Structure>(key)
                .ok()
                .map(|s| (key.to_string(), s))
        })
        .collect();
    let mut audio = Vec::new();
    for (_, row) in &rows {
        let kind = match row.get::<T>("type").ok() {
            Some(T::OutboundRtp) => "outbound-rtp",
            Some(T::InboundRtp) => "inbound-rtp",
            Some(T::RemoteInboundRtp) => "remote-inbound-rtp",
            _ => continue,
        };
        let mut result = json!({"type":kind});
        for (source, target) in [
            ("packets-sent", "packetsSent"),
            ("packets-received", "packetsReceived"),
            ("bytes-sent", "bytesSent"),
            ("bytes-received", "bytesReceived"),
            ("packets-lost", "packetsLost"),
            ("jitter", "jitter"),
            ("round-trip-time", "roundTripTime"),
            ("audio-level", "audioLevel"),
        ] {
            if let Some(value) = numeric(row, source) {
                result[target] = value;
            }
        }
        audio.push(result);
    }
    let selected = rows
        .iter()
        .find_map(|(_, row)| row.get::<String>("selected-candidate-pair-id").ok());
    let pair = selected.and_then(|id| rows.iter().find(|(key, _)| key == &id).map(|(_, row)| row));
    let route = pair.map(|pair| {
        let lookup = |key| pair.get::<String>(key).ok().and_then(|id|
            rows.iter().find(|(key, _)| key == &id).map(|(_, row)| candidate(row)));
        json!({"local":lookup("local-candidate-id"), "remote":lookup("remote-candidate-id"),
            "roundTripTime":numeric(pair, "current-round-trip-time"),
            "bytesSent":numeric(pair, "bytes-sent"), "bytesReceived":numeric(pair, "bytes-received")})
    });
    json!({"audio":audio, "route":route})
}

pub struct Diagnostics {
    enabled: AtomicBool,
    started: Instant,
    started_unix_ms: u128,
    report: Mutex<Report>,
}

#[derive(Default)]
struct Report {
    aliases: BTreeMap<String, String>,
    events: VecDeque<Value>,
    samples: VecDeque<Value>,
    latest: BTreeMap<String, Value>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            started: Instant::now(),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            report: Mutex::new(Report::default()),
        }
    }
}

fn alias(report: &mut Report, id: &str) -> String {
    let next = format!("peer-{}", report.aliases.len() + 1);
    report.aliases.entry(id.into()).or_insert(next).clone()
}

fn push(queue: &mut VecDeque<Value>, value: Value) {
    if queue.len() == LIMIT {
        queue.pop_front();
    }
    queue.push_back(value);
}

impl Diagnostics {
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn begin_session(&self) {
        self.report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest
            .clear();
        self.event("join-started", None);
    }

    pub fn set_enabled(&self, enabled: bool) {
        // Record the switch itself even when disabling collection.
        self.enabled.store(true, Ordering::Relaxed);
        self.event(
            if enabled {
                "diagnostics-enabled"
            } else {
                "diagnostics-disabled"
            },
            None,
        );
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn event(&self, code: &'static str, id: Option<&str>) {
        if !self.enabled() {
            return;
        }
        let mut report = self.report.lock().unwrap_or_else(|e| e.into_inner());
        let peer = id.map(|id| alias(&mut report, id));
        if code == "peer-left" {
            if let Some(peer) = &peer {
                report.latest.remove(peer);
            }
        }
        push(
            &mut report.events,
            json!({"ms": self.started.elapsed().as_millis(), "code":code, "peer":peer}),
        );
    }

    pub fn sample(&self, id: &str, mut value: Value) {
        if !self.enabled() {
            return;
        }
        let mut report = self.report.lock().unwrap_or_else(|e| e.into_inner());
        let peer = alias(&mut report, id);
        value["peer"] = json!(peer);
        value["ms"] = json!(self.started.elapsed().as_millis());
        report.latest.insert(peer, value.clone());
        push(&mut report.samples, value);
    }

    pub fn export(&self) -> Value {
        let report = self.report.lock().unwrap_or_else(|e| e.into_inner());
        json!({
            "schemaVersion":1, "client":"linux", "version":env!("CARGO_PKG_VERSION"),
            "gstreamerVersion":gst::version(),
            "startedAtUnixMs":self.started_unix_ms,
            "exportedAtUnixMs":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
            "enabled":self.enabled(), "limits":{"samples":LIMIT,"events":LIMIT,"intervalMs":1000},
            "events":report.events, "samples":report.samples,
        })
    }

    pub fn summary(&self) -> String {
        let report = self.report.lock().unwrap_or_else(|e| e.into_inner());
        if report.latest.is_empty() {
            return "Ожидаем данные. Включите диагностику перед подключением к собеседнику.".into();
        }
        report.latest.iter().map(|(peer, sample)| {
            let packets = |kind: &str, field: &str| -> String {
                let values: Vec<f64> = sample["audio"].as_array().into_iter().flatten()
                    .filter(|row| row["type"] == kind).filter_map(|row| row[field].as_f64()).collect();
                if values.is_empty() { "н/д".into() } else { format!("{:.0}", values.iter().sum::<f64>()) }
            };
            let text = |key: &str| sample[key].as_str().unwrap_or("н/д");
            let route = format!("{} → {}",
                sample["route"]["local"]["type"].as_str().unwrap_or("н/д"),
                sample["route"]["remote"]["type"].as_str().unwrap_or("н/д"));
            let rms = sample["microphone"]["rms"].as_f64().unwrap_or(0.0);
            let age = sample["microphone"]["ageMs"].as_u64().map(|n| format!("{n} мс назад")).unwrap_or("нет данных".into());
            let output = &sample["playback"]["buffers"];
            format!("{peer}: {} · ICE {}\nМикрофон RMS {rms:.4} ({age}) · mute {}\nRTP пакеты ↑ {} / ↓ {}\nМаршрут: {route} · буферов устройству: {output}",
                text("connection"), text("ice"), sample["microphone"]["muted"],
                packets("outbound-rtp", "packetsSent"), packets("inbound-rtp", "packetsReceived"))
        }).collect::<Vec<_>>().join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gst_stats_strip_addresses_credentials_and_ids() {
        gst::init().unwrap();
        let out = gst::Structure::builder("outbound-rtp")
            .field("type", gst_webrtc::WebRTCStatsType::OutboundRtp)
            .field("packets-sent", 42_u64)
            .field("bytes-sent", 800_u64)
            .field("ssrc", "private-ssrc")
            .build();
        let transport = gst::Structure::builder("transport")
            .field("selected-candidate-pair-id", "private-pair")
            .build();
        let pair = gst::Structure::builder("candidate-pair")
            .field("local-candidate-id", "private-candidate")
            .field("current-round-trip-time", 0.02_f64)
            .build();
        let local = gst::Structure::builder("local-candidate")
            .field("candidate-type", "relay")
            .field("protocol", "udp")
            .field("address", "198.51.100.1")
            .field("url", "turn:private.example")
            .field("username", "private-user")
            .field("credential", "private-key")
            .build();
        let reply = gst::Structure::builder("stats")
            .field("out", out)
            .field("transport", transport)
            .field("private-pair", pair)
            .field("private-candidate", local)
            .build();
        let projected = project_stats(&reply);
        assert_eq!(projected["audio"][0]["packetsSent"], 42);
        assert_eq!(projected["route"]["local"]["type"], "relay");
        assert!(!projected.to_string().contains("private"));
        assert!(!projected.to_string().contains("198.51"));
    }

    #[test]
    fn opt_in_bounded_and_anonymous_across_reconnects() {
        let d = Diagnostics::default();
        d.event("session-joined", None);
        d.sample("private-peer-id", json!({"audio":[]}));
        assert!(d.export()["samples"].as_array().unwrap().is_empty());
        d.set_enabled(true);
        for n in 0..350 {
            d.sample("private-peer-id", json!({"counter":n}));
            d.event("connection-state-changed", Some("private-peer-id"));
        }
        d.event("session-left", None);
        d.sample("new-private-peer-id", json!({"audio":[]}));
        d.set_enabled(false);
        d.sample("ignored", json!({}));
        let report = d.export();
        assert_eq!(report["samples"].as_array().unwrap().len(), LIMIT);
        assert_eq!(report["events"].as_array().unwrap().len(), LIMIT);
        assert_eq!(report["samples"][LIMIT - 1]["peer"], "peer-2");
        assert!(!report.to_string().contains("private-peer-id"));
        assert!(!report.to_string().contains("ignored"));
    }
}
