import test from 'node:test';
import assert from 'node:assert/strict';
import { Diagnostics, projectStats } from '../web/diagnostics.js';

function stats() {
  return new Map([
    ['out', { type: 'outbound-rtp', kind: 'audio', packetsSent: 42, bytesSent: 800, ssrc: 'private-ssrc' }],
    ['in', { type: 'inbound-rtp', mediaType: 'audio', packetsReceived: 31, jitter: 0.01, packetsLost: 2 }],
    ['video', { type: 'outbound-rtp', kind: 'video', packetsSent: 999 }],
    ['source', { type: 'media-source', kind: 'audio', audioLevel: 0.2, deviceIdentifier: 'private-device' }],
    ['transport', { type: 'transport', selectedCandidatePairId: 'pair' }],
    ['old', { type: 'candidate-pair', nominated: true, state: 'succeeded', localCandidateId: 'host' }],
    ['pair', { type: 'candidate-pair', localCandidateId: 'relay', remoteCandidateId: 'host', currentRoundTripTime: 0.02 }],
    ['relay', { type: 'local-candidate', candidateType: 'relay', protocol: 'udp', address: '198.51.100.1', url: 'turn:private.example', username: 'private-user', credential: 'private-key' }],
    ['host', { type: 'remote-candidate', candidateType: 'host', protocol: 'udp', address: '192.168.1.1', port: 12345 }],
    ['cert', { type: 'certificate', base64Certificate: 'private-certificate' }],
  ]);
}

test('stats allowlist retains directional counters and the selected route, strips identifiers', () => {
  const safe = projectStats(stats());
  assert.equal(safe.audio.length, 3);
  assert.equal(safe.audio[0].packetsSent, 42);
  assert.equal(safe.audio[1].packetsReceived, 31);
  assert.equal(safe.route.local.type, 'relay');
  assert.equal(safe.route.remote.type, 'host');
  assert.equal(safe.route.roundTripTime, 0.02);
  assert.doesNotMatch(JSON.stringify(safe), /private|198\.51|192\.168|ssrc|12345|999/);
});

test('missing browser stats are unknown, never invented zeroes', () => {
  assert.deepEqual(projectStats(new Map()), { audio: [], route: null });
  assert.deepEqual(projectStats(new Map([['in', { type: 'inbound-rtp', kind: 'audio', jitter: NaN }]])), {
    audio: [{ type: 'inbound-rtp' }], route: null,
  });
});

test('collection is opt-in, bounded, anonymous and retained across reconnects and disabling', async () => {
  const d = new Diagnostics();
  let calls = 0;
  const peer = {
    id: 'private-participant-id', name: 'private-name', muted: false,
    pc: { connectionState: 'connected', iceConnectionState: 'completed', signalingState: 'stable',
      async getStats() { calls++; return stats(); } },
    audio: { paused: true, muted: false, volume: 1, readyState: 4, currentTime: 12,
      error: { code: 4, message: 'private-error' } },
  };
  const peers = new Map([[peer.id, peer]]);
  const stream = { getAudioTracks: () => [{ enabled: true, muted: false, readyState: 'live', label: 'private-microphone' }] };
  await d.sample(stream, peers, { state: 'running' });
  assert.equal(calls, 0);
  d.setEnabled(true);
  for (let i = 0; i < 310; i++) {
    d.event('connection-state-changed', peer.id);
    await d.sample(stream, peers, { state: 'running' }, 0.1);
  }
  d.event('session-left');
  peer.id = 'private-new-id';
  await d.sample(stream, new Map([[peer.id, peer]]), { state: 'running' });
  d.setEnabled(false);
  await d.sample(stream, peers, null);
  const report = d.report();
  assert.equal(calls, 311);
  assert.equal(report.samples.length, 300);
  assert.equal(report.events.length, 300);
  assert.equal(report.samples.at(-1).peers[0].peer, 'peer-2');
  assert.equal(report.samples.at(-1).peers[0].playback.paused, true);
  assert.doesNotMatch(JSON.stringify(report), /private|192\.168|198\.51/);
});

test('a rejected stats request and disabling during a request do not break collection', async () => {
  const d = new Diagnostics();
  d.setEnabled(true);
  const peer = { id: 'id', pc: { getStats: async () => { throw new Error('private-error'); } } };
  await d.sample(null, new Map([['id', peer]]), null);
  assert.equal(d.report().samples[0].peers[0].statsUnavailable, true);
  let finish;
  peer.pc.getStats = () => new Promise((resolve) => { finish = resolve; });
  const pending = d.sample(null, new Map([['id', peer]]), null);
  d.setEnabled(false);
  finish(stats());
  await pending;
  assert.equal(d.report().samples.length, 1);
});
