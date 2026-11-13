import { accessAllowed, accessExpired } from '/auth.js';

const $ = (id) => document.getElementById(id);
const room = new URLSearchParams(location.search).get('room') || makeRoom();
const validRoom = /^[a-z0-9][a-z0-9-]{0,63}$/.test(room);
const peers = new Map();
let self = null;
let stream = null;
let eventsAbort = null;
let audioContext = null;
let meters = new Map();
let meterTimer = null;
let muted = false;
let config;

function makeRoom() {
  const bytes = crypto.getRandomValues(new Uint8Array(12));
  const id = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  history.replaceState(null, '', `?room=${id}`);
  return id;
}

function message(text = '', error = false) {
  $('message').textContent = text;
  $('message').classList.toggle('error', error);
}

async function api(path, data, session = self) {
  const response = await fetch(`/api/${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...(session ? { Authorization: `Bearer ${session.token}` } : {}) },
    body: JSON.stringify(data),
  });
  if (!response.ok) {
    if (response.status === 401 && response.headers.get('WWW-Authenticate')?.startsWith('MamlaKey')) accessExpired();
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || `Сервер ответил ${response.status}`);
  }
  return response.status === 204 ? null : response.json();
}

function tile(person, local = false) {
  const el = document.createElement('div');
  el.className = 'participant';
  el.dataset.id = person.id;
  const avatar = document.createElement('div');
  avatar.className = 'avatar';
  avatar.textContent = Array.from(person.name)[0].toLocaleUpperCase();
  const name = document.createElement('div');
  name.className = 'participant-name';
  name.textContent = person.name + (local ? ' · вы' : '');
  name.title = person.name;
  const status = document.createElement('div');
  status.className = 'participant-status';
  status.textContent = local ? 'Микрофон включён' : 'Подключение…';
  el.append(avatar, name, status);
  $('participants').append(el);
  return el;
}

function updateCount() {
  $('count').textContent = `${peers.size + 1} из ${config.roomLimit} участников`;
  $('connection').textContent = peers.size ? 'Вы в комнате' : 'Ждём компанию';
}

function peerStatus(peer) {
  const state = peer.pc?.connectionState;
  peer.el.querySelector('.participant-status').textContent =
    peer.muted ? 'Микрофон выключен' :
    state === 'connected' ? 'На связи' :
    state === 'failed' ? 'Нет связи · перезайдите' :
    state === 'disconnected' ? 'Связь прервалась' : 'Подключение…';
}

function addPeer(person) {
  if (peers.has(person.id)) return peers.get(person.id);
  const peer = { ...person, el: tile(person), pc: null, candidates: [], audio: null };
  peers.set(person.id, peer);
  updateCount();
  return peer;
}

function monitor(id, media, el) {
  if (!audioContext) return;
  const source = audioContext.createMediaStreamSource(media);
  const analyser = audioContext.createAnalyser();
  analyser.fftSize = 256;
  source.connect(analyser);
  meters.set(id, { source, analyser, samples: new Uint8Array(analyser.fftSize), el });
}

function removePeer(id) {
  const peer = peers.get(id);
  if (!peer) return;
  peers.delete(id);
  peer.pc?.close();
  if (peer.audio) { peer.audio.pause(); peer.audio.srcObject = null; peer.audio.remove(); }
  const meter = meters.get(id);
  meter?.source.disconnect();
  meters.delete(id);
  peer.el.remove();
  updateCount();
}

async function signal(peer, type, data) {
  await api('signal', { to: peer.id, type, data });
}

function connection(peer) {
  if (peer.pc) return peer.pc;
  const pc = new RTCPeerConnection({ iceServers: config.iceServers });
  peer.pc = pc;
  for (const track of stream.getTracks()) pc.addTrack(track, stream);
  pc.onicecandidate = ({ candidate }) => {
    if (candidate && self) signal(peer, 'ice', candidate.toJSON()).catch(() => {
      if (peers.has(peer.id)) message('Не удалось передать ICE-кандидат. Если звук не появился, перезайдите.', true);
    });
  };
  pc.onconnectionstatechange = () => { if (peers.has(peer.id)) peerStatus(peer); };
  pc.ontrack = ({ streams }) => {
    if (peer.audio || !streams[0]) return;
    const audio = document.createElement('audio');
    audio.autoplay = true;
    audio.srcObject = streams[0];
    peer.audio = audio;
    document.body.append(audio);
    audio.play().catch(() => { $('play').hidden = false; });
    monitor(peer.id, streams[0], peer.el);
  };
  return pc;
}

async function offer(person) {
  const peer = addPeer(person);
  const pc = connection(peer);
  await pc.setLocalDescription(await pc.createOffer());
  await signal(peer, 'offer', pc.localDescription);
}

async function receiveSignal(data) {
  const peer = peers.get(data.from);
  if (!peer) return; // A participant may have left with signals still in flight.
  const pc = connection(peer);
  if (data.type === 'ice') {
    if (pc.remoteDescription) await pc.addIceCandidate(data.data);
    else peer.candidates.push(data.data);
    return;
  }
  if (data.type !== 'offer' && data.type !== 'answer') return;
  if (data.data?.type !== data.type) throw new Error('Некорректный SDP');
  await pc.setRemoteDescription(data.data);
  for (const candidate of peer.candidates.splice(0)) await pc.addIceCandidate(candidate);
  if (data.type === 'offer') {
    await pc.setLocalDescription(await pc.createAnswer());
    await signal(peer, 'answer', pc.localDescription);
  }
}

async function onEvent(event) {
  const { type, data } = event;
  if (type === 'ready') {
    for (const person of data.peers) await offer(person);
    updateCount();
  } else if (type === 'peer-joined') {
    addPeer(data); // Only the newcomer creates offers, preventing negotiation glare.
  } else if (type === 'peer-left') {
    removePeer(data.id);
  } else if (type === 'peer-updated') {
    const peer = peers.get(data.id);
    if (peer) { peer.muted = data.muted; peerStatus(peer); }
  } else if (type === 'signal') {
    await receiveSignal(data);
  }
}

async function listen(session, abort) {
  const response = await fetch('/api/events', {
    headers: { Authorization: `Bearer ${session.token}` }, signal: abort.signal,
  });
  if (response.status === 401 && response.headers.get('WWW-Authenticate')?.startsWith('MamlaKey')) accessExpired();
  if (!response.ok || !response.body) throw new Error('Не удалось подключиться к комнате');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) throw new Error('Соединение с сервером потеряно. Войдите снова.');
      buffer += decoder.decode(value, { stream: true });
      let boundary;
      while ((boundary = buffer.indexOf('\n\n')) !== -1) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        if (self !== session) return;
        if (frame.startsWith('data: ')) {
          try { await onEvent(JSON.parse(frame.slice(6))); }
          catch { message('Не удалось соединиться с одним из участников. Попробуйте войти снова.', true); }
        }
      }
    }
  } finally { reader.releaseLock(); }
}

function friendlyError(error) {
  if (error.name === 'TypeError' && /load failed|failed to fetch|networkerror/i.test(error.message)) {
    return 'Не удалось выполнить сетевой запрос к серверу. Проверьте доступность сервера и доверие HTTPS-сертификату в браузере. Для самоподписанного сертификата Safari может потребовать добавить публичный сертификат в «Связку ключей» и разрешить доверие.';
  }
  if (error.name === 'NotAllowedError') return 'Доступ к микрофону запрещён. Разрешите его в настройках браузера.';
  if (error.name === 'NotFoundError') return 'Микрофон не найден. Подключите его и попробуйте снова.';
  if (error.name === 'NotReadableError') return 'Не удалось открыть микрофон. Проверьте, не занят ли он другим приложением.';
  return error.message || 'Не удалось войти в комнату.';
}

function leave(notify = true) {
  const session = self;
  self = null;
  eventsAbort?.abort();
  eventsAbort = null;
  if (session && notify) {
    fetch('/api/leave', { method: 'POST', headers: { Authorization: `Bearer ${session.token}` }, keepalive: true }).catch(() => {});
  }
  stream?.getTracks().forEach((track) => track.stop());
  stream = null;
  for (const id of [...peers.keys()]) removePeer(id);
  for (const meter of meters.values()) meter.source.disconnect();
  meters.clear();
  clearInterval(meterTimer);
  audioContext?.close().catch(() => {});
  audioContext = null;
  $('participants').replaceChildren();
  $('join-form').hidden = false;
  $('call').hidden = true;
  $('play').hidden = true;
  $('join').disabled = false;
  $('mute').disabled = false;
  $('mute').textContent = 'Выключить микрофон';
  $('mute').setAttribute('aria-pressed', 'false');
  message();
}

$('join-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (self || $('join').disabled) return;
  $('join').disabled = true;
  message('Подключаем микрофон…');
  try {
    if (!await accessAllowed()) { $('join').disabled = false; return; }
    if (!isSecureContext || !navigator.mediaDevices?.getUserMedia) {
      throw new Error('Для микрофона откройте приложение через HTTPS или localhost.');
    }
    if (!validRoom) throw new Error('Некорректная ссылка комнаты. Откройте главную страницу.');
    const response = await fetch('/api/config');
    if (!response.ok) {
      if (response.status === 401 && response.headers.get('WWW-Authenticate')?.startsWith('MamlaKey')) accessExpired();
      throw new Error('Сервис недоступен или требуется повторный вход.');
    }
    config = await response.json();
    stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true }, video: false });
    const name = $('name').value.trim();
    self = await api('join', { room, name });
    const session = self;
    muted = false;
    try {
      audioContext = new AudioContext();
      await audioContext.resume();
    } catch { audioContext = null; }
    const local = tile({ id: self.id, name: self.name || name }, true);
    monitor(self.id, stream, local);
    meterTimer = setInterval(() => {
      for (const [id, meter] of meters) {
        meter.analyser.getByteTimeDomainData(meter.samples);
        const rms = Math.sqrt(meter.samples.reduce((sum, n) => sum + ((n - 128) / 128) ** 2, 0) / meter.samples.length);
        const isMuted = id === self?.id ? muted : peers.get(id)?.muted;
        meter.el.classList.toggle('speaking', !isMuted && rms > 0.025);
      }
    }, 100);
    $('join-form').hidden = true;
    $('call').hidden = false;
    updateCount();
    message();
    eventsAbort = new AbortController();
    listen(session, eventsAbort).catch((error) => {
      if (self === session && error.name !== 'AbortError') {
        // Closing SSE already removes this session on the server. Avoid a
        // redundant leave request that would report 401 for the expired token.
        leave(false);
        message(friendlyError(error), true);
      }
    });
    stream.getAudioTracks()[0].onended = () => {
      if (self === session) { leave(); message('Микрофон отключился. Подключите его и войдите снова.', true); }
    };
  } catch (error) {
    leave();
    message(friendlyError(error), true);
  }
});

$('mute').addEventListener('click', async () => {
  if (!self || !stream) return;
  const session = self;
  muted = !muted;
  for (const track of stream.getAudioTracks()) track.enabled = !muted;
  $('mute').textContent = muted ? 'Включить микрофон' : 'Выключить микрофон';
  $('mute').setAttribute('aria-pressed', String(muted));
  document.querySelector(`[data-id="${self.id}"] .participant-status`).textContent = muted ? 'Микрофон выключен' : 'Микрофон включён';
  $('mute').disabled = true;
  try { await api('mute', { muted }, session); }
  catch { if (self === session) message('Микрофон переключён, но статус не доставлен участникам.', true); }
  finally { if (self === session) $('mute').disabled = false; }
});

$('leave').addEventListener('click', () => { leave(); message('Вы вышли из комнаты. Можно вернуться в любой момент.'); });
$('copy').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(location.href);
    message('Ссылка скопирована. Отправьте её своим.');
  } catch { message(`Ссылка на комнату: ${location.href}`); }
});
$('play').addEventListener('click', async () => {
  try {
    await audioContext?.resume();
    await Promise.all([...peers.values()].filter((p) => p.audio).map((p) => p.audio.play()));
    $('play').hidden = true;
  } catch { message('Браузер пока блокирует звук. Нажмите ещё раз.', true); }
});
window.addEventListener('pagehide', () => leave());
window.addEventListener('mamla:access-lost', () => leave(false));

if (!validRoom) {
  message('Некорректная ссылка комнаты. Откройте главную страницу.', true);
  $('join').disabled = true;
} else {
  $('room-heading').textContent = `Комната ${room.slice(0, 8)}`;
}
