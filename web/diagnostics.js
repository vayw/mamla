const LIMIT = 300;
const number = (value) => typeof value === 'number' && Number.isFinite(value) ? value : null;
const choice = (value, choices) => choices.includes(value) ? value : 'unknown';
const states = ['new', 'checking', 'connecting', 'connected', 'completed', 'disconnected', 'failed', 'closed', 'stable', 'have-local-offer', 'have-remote-offer', 'have-local-pranswer', 'have-remote-pranswer'];

function environment() {
  const ua = globalThis.navigator?.userAgent || '';
  return {
    browser: globalThis.navigator?.brave ? 'brave' : /Firefox\//.test(ua) ? 'firefox'
      : /Edg\//.test(ua) ? 'edge' : /Chrome\/|Chromium\/|CriOS\//.test(ua) ? 'chromium'
        : /Safari\//.test(ua) ? 'safari' : 'unknown',
    os: /Android/.test(ua) ? 'android' : /iPhone|iPad/.test(ua) ? 'ios'
      : /Macintosh/.test(ua) ? 'macos' : /Linux/.test(ua) ? 'linux' : /Windows/.test(ua) ? 'windows' : 'unknown',
  };
}

// Deliberately project an allowlist. Raw stats include IPs, URLs and device IDs.
export function projectStats(report) {
  const rows = [...report.values()];
  const audio = rows.filter((s) => ['inbound-rtp', 'outbound-rtp', 'remote-inbound-rtp', 'media-source'].includes(s.type)
    && (s.kind === 'audio' || s.mediaType === 'audio'));
  const transport = rows.find((s) => s.type === 'transport' && s.selectedCandidatePairId);
  const pair = (transport && report.get(transport.selectedCandidatePairId))
    || rows.find((s) => s.type === 'candidate-pair' && s.selected)
    || rows.find((s) => s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded');
  const candidate = (id) => {
    const s = report.get(id);
    return s ? {
      type: choice(s.candidateType, ['host', 'srflx', 'prflx', 'relay']),
      protocol: choice(s.protocol, ['udp', 'tcp']),
    } : null;
  };
  return {
    audio: audio.map((s) => {
      const result = { type: s.type };
      for (const key of ['packetsSent', 'packetsReceived', 'bytesSent', 'bytesReceived', 'packetsLost', 'jitter',
        'roundTripTime', 'audioLevel', 'totalAudioEnergy', 'totalSamplesDuration', 'concealedSamples', 'silentConcealedSamples']) {
        if (number(s[key]) !== null) result[key] = s[key];
      }
      return result;
    }),
    route: pair ? {
      local: candidate(pair.localCandidateId), remote: candidate(pair.remoteCandidateId),
      roundTripTime: number(pair.currentRoundTripTime),
      bytesSent: number(pair.bytesSent), bytesReceived: number(pair.bytesReceived),
    } : null,
  };
}

function trackState(track) {
  return {
    enabled: !!track.enabled, muted: !!track.muted,
    readyState: choice(track.readyState, ['live', 'ended']),
  };
}

export class Diagnostics {
  constructor() {
    this.enabled = false;
    this.started = Date.now();
    this.events = [];
    this.samples = [];
    this.aliases = new Map();
    this.generation = 0;
    this.busy = false;
  }
  setEnabled(enabled) {
    this.generation++;
    this.enabled = enabled;
    this.event(enabled ? 'diagnostics-enabled' : 'diagnostics-disabled', null, true);
  }
  alias(id) {
    if (!this.aliases.has(id)) this.aliases.set(id, `peer-${this.aliases.size + 1}`);
    return this.aliases.get(id);
  }
  event(code, id = null, force = false, state = null) {
    if (!this.enabled && !force) return;
    this.events.push({ ms: Date.now() - this.started, code, peer: id ? this.alias(id) : null,
      ...(state === null ? {} : { state: choice(state, states) }) });
    if (this.events.length > LIMIT) this.events.shift();
  }
  async sample(stream, peers, audioContext, microphoneRms = null) {
    if (!this.enabled || this.busy) return;
    this.busy = true;
    const generation = this.generation;
    const snapshot = {
      ms: Date.now() - this.started,
      microphone: stream ? stream.getAudioTracks().map(trackState) : [],
      microphoneRms: number(microphoneRms),
      audioContext: choice(audioContext?.state, ['running', 'suspended', 'interrupted', 'closed']),
      peers: [],
    };
    try {
      await Promise.all([...peers.values()].map(async (peer) => {
        if (!peer.pc) return;
        const row = {
          peer: this.alias(peer.id),
          connection: choice(peer.pc.connectionState, states),
          ice: choice(peer.pc.iceConnectionState, states),
          signaling: choice(peer.pc.signalingState, states),
          remoteMuted: !!peer.muted,
          playback: peer.audio ? {
            paused: !!peer.audio.paused, muted: !!peer.audio.muted,
            volume: number(peer.audio.volume), readyState: number(peer.audio.readyState),
            currentTime: number(peer.audio.currentTime),
            errorCode: number(peer.audio.error?.code),
          } : null,
        };
        try {
          // Bound a browser implementation that never settles getStats().
          let timeout;
          try {
            const report = await Promise.race([
              peer.pc.getStats(),
              new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error()), 1500); }),
            ]);
            Object.assign(row, projectStats(report));
          } finally { clearTimeout(timeout); }
        } catch { row.statsUnavailable = true; }
        snapshot.peers.push(row);
      }));
      if (this.enabled && this.generation === generation) {
        this.samples.push(snapshot);
        if (this.samples.length > LIMIT) this.samples.shift();
      }
    } finally { this.busy = false; }
  }
  report() {
    return {
      schemaVersion: 1, client: 'browser', startedAt: new Date(this.started).toISOString(),
      environment: environment(),
      exportedAt: new Date().toISOString(), enabled: this.enabled,
      limits: { samples: LIMIT, events: LIMIT, intervalMs: 1000 },
      events: this.events, samples: this.samples,
    };
  }
  summary() {
    const sample = this.samples.at(-1);
    if (!sample) return 'Ожидаем данные. Включите диагностику перед подключением.';
    const mic = sample.microphone[0];
    const lines = [`Микрофон: ${!mic ? 'не открыт' : !mic.enabled ? 'выключен' : mic.muted ? 'не подаёт данные' : mic.readyState}`,
      `Уровень микрофона RMS: ${sample.microphoneRms === null ? 'н/д' : sample.microphoneRms.toFixed(4)}`,
      `Аудиоконтекст: ${sample.audioContext}`];
    for (const p of sample.peers) {
      const sum = (key, type) => {
        const rows = (p.audio || []).filter((s) => s.type === type && typeof s[key] === 'number');
        return rows.length ? rows.reduce((n, s) => n + s[key], 0) : 'н/д';
      };
      const level = (p.audio || []).find((s) => s.type === 'media-source' && s.audioLevel !== undefined)?.audioLevel;
      lines.push(`${p.peer}: ${p.connection} · ICE ${p.ice}`,
        `  RTP пакеты ↑ ${sum('packetsSent', 'outbound-rtp')} / ↓ ${sum('packetsReceived', 'inbound-rtp')}`,
        `  Уровень микрофона: ${level === undefined ? 'н/д' : level.toFixed(4)}`,
        `  Маршрут: ${p.route ? `${p.route.local?.type || '?'} → ${p.route.remote?.type || '?'}` : 'н/д'}`,
        `  Воспроизведение: ${!p.playback ? 'нет дорожки' : p.playback.paused ? 'на паузе / заблокировано' : 'запущено'}${p.statsUnavailable ? ' · статистика недоступна' : ''}`);
    }
    return lines.join('\n');
  }
}
