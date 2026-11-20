import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import { Diagnostics } from '../web/diagnostics.js';

// Exercise the real event handlers without microphone/device permissions.
async function fixture({ blockPlayback = false } = {}) {
  const nodes = new Map();
  const appended = [];
  const document = {
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, {
        hidden: true, listeners: new Map(),
        addEventListener(type, fn) { this.listeners.set(type, fn); },
      });
      return nodes.get(id);
    },
    createElement(tag) {
      assert.equal(tag, 'audio');
      return { play: async () => {
        if (blockPlayback) throw new Error('autoplay blocked');
      } };
    },
    body: { append(audio) { appended.push(audio); } },
  };
  const context = vm.createContext({
    document, URLSearchParams, Diagnostics, location: { search: '?room=test' },
    window: { addEventListener() {} },
    RTCPeerConnection: class { addTrack() {} },
    MediaStream: class {
      constructor(tracks) { this.tracks = tracks; }
      getAudioTracks() { return this.tracks; }
    },
  });
  const source = await readFile(new URL('../web/app.js', import.meta.url), 'utf8');
  vm.runInContext(source.replace(/^import .*;\n/gm, '') + `
    config = { iceServers: [] };
    stream = { getTracks: () => [] };
    globalThis.handlers = { connection, peers,
      setContext(value) { audioContext = value; }
    };
  `, context);
  const peer = { id: 'friend', audio: null };
  context.handlers.peers.set(peer.id, peer);
  return { nodes, appended, peer, pc: context.handlers.connection(peer), handlers: context.handlers };
}

test('remote audio plays with or without a MediaStream association', async () => {
  for (const streamless of [false, true]) {
    const f = await fixture();
    const track = { kind: 'audio' };
    const media = { getAudioTracks: () => [track] };
    f.pc.ontrack({ track, streams: streamless ? [] : [media] });
    assert.equal(f.appended.length, 1);
    assert.equal(f.peer.audio.srcObject.getAudioTracks()[0], track);
    if (!streamless) assert.equal(f.peer.audio.srcObject, media);
    // A repeated track event must not create overlapping playback elements.
    f.pc.ontrack({ track, streams: [] });
    assert.equal(f.appended.length, 1);
  }
});

test('blocked autoplay exposes the manual playback button', async () => {
  const f = await fixture({ blockPlayback: true });
  f.pc.ontrack({ track: { kind: 'audio' }, streams: [{}] });
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(f.nodes.get('play').hidden, false);
});

test('manual playback starts before AudioContext resume resolves', async () => {
  const f = await fixture();
  let finishResume;
  f.handlers.setContext({ resume: () => new Promise((resolve) => { finishResume = resolve; }) });
  let plays = 0;
  f.peer.audio = { play: async () => { plays++; } };
  const click = f.nodes.get('play').listeners.get('click')();
  assert.equal(plays, 1, 'play must run synchronously in the click handler');
  finishResume();
  await click;
  assert.equal(f.nodes.get('play').hidden, true);
});
