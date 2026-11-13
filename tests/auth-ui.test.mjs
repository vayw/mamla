import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

// DOM stubs exercise module state and requests; these are not browser layout tests.
async function fixture({ loseAcceptResponse = false, enabled = true } = {}) {
  const html = await readFile(new URL('../web/index.html', import.meta.url), 'utf8');
  const nodes = new Map([...html.matchAll(/\bid="([^"]+)"/g)].map((m) => [m[1], {
    hidden: true, disabled: false, value: '', textContent: '', listeners: new Map(),
    addEventListener(type, fn) { this.listeners.set(type, fn); },
  }]));
  nodes.get('access-message').hidden = false;
  globalThis.document = { getElementById(id) {
    assert.ok(nodes.has(id), `missing real HTML element ${id}`);
    return nodes.get(id);
  } };
  globalThis.location = new URL('https://mamla.example/?room=friends#invite=mi_test-invite');
  globalThis.history = { replaceState(_state, _title, path) { globalThis.location = new URL(path, location); } };
  globalThis.window = new EventTarget();
  let authenticated = false;
  let acceptedKey;
  const calls = [];
  globalThis.fetch = async (path, options = {}) => {
    calls.push({ path, ...options });
    if (path === '/api/auth/status') return Response.json({ enabled, authenticated, name: authenticated ? 'Friend' : '' });
    const body = JSON.parse(options.body);
    if (path === '/api/auth/accept') {
      assert.equal(body.invite, 'mi_test-invite');
      assert.match(body.key, /^mk_[A-Za-z0-9_-]{43}$/);
      acceptedKey = body.key;
      authenticated = true;
      if (loseAcceptResponse) { loseAcceptResponse = false; throw new TypeError('Load failed'); }
    } else if (path === '/api/auth/login') {
      assert.equal(body.key, acceptedKey);
      authenticated = true;
    } else if (path === '/api/auth/logout') authenticated = false;
    else assert.fail('unexpected API path');
    return new Response(null, { status: 204 });
  };
  const module = await import(`../web/auth.js?test=${Math.random()}`);
  await module.accessAllowed();
  return { nodes, calls, module, async click(id) {
    await nodes.get(id).listeners.get('click')();
  } };
}

test('preview does not consume invite; acceptance creates local key and supports logout', async () => {
  const f = await fixture();
  assert.equal(location.hash, '');
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].path, '/api/auth/status');
  assert.equal(await f.module.accessAllowed(), false);
  await f.click('accept-invite');
  assert.equal(await f.module.accessAllowed(), true);
  assert.equal(f.nodes.get('room-panel').hidden, false);
  assert.equal(f.nodes.get('nickname-fields').hidden, true);
  assert.equal(f.nodes.get('name').disabled, true);
  assert.equal(f.nodes.get('name').required, false);
  assert.equal(f.nodes.get('name').value, 'Friend');
  assert.equal(f.nodes.get('key-panel').hidden, false);
  const key = f.nodes.get('generated-key').value;
  assert.match(key, /^mk_[A-Za-z0-9_-]{43}$/);
  assert.ok(!location.href.includes(key));
  for (const call of f.calls) assert.ok(!call.path.includes(key));
  await f.click('hide-key');
  assert.equal(await f.module.accessAllowed(), true);
  assert.equal(f.nodes.get('generated-key').value, '');
  assert.equal(f.nodes.get('key-panel').hidden, true);
  await f.click('service-logout');
  assert.equal(await f.module.accessAllowed(), false);
  assert.equal(f.nodes.get('generated-key').value, '');
  assert.equal(f.nodes.get('key-panel').hidden, true);
});

test('open service still asks for a nickname', async () => {
  const f = await fixture({ enabled: false });
  assert.equal(await f.module.accessAllowed(), true);
  assert.equal(f.nodes.get('nickname-fields').hidden, false);
  assert.equal(f.nodes.get('name').disabled, false);
  assert.equal(f.nodes.get('name').required, true);
});

test('a lost accept response preserves the locally generated key for recovery', async () => {
  const f = await fixture({ loseAcceptResponse: true });
  await f.click('accept-invite');
  const key = f.nodes.get('generated-key').value;
  assert.match(key, /^mk_[A-Za-z0-9_-]{43}$/);
  assert.equal(f.nodes.get('key-panel').hidden, false);
  assert.equal(await f.module.accessAllowed(), false);
  await f.click('login-generated-key');
  assert.equal(await f.module.accessAllowed(), true);
  assert.equal(f.nodes.get('generated-key').value, key);
  assert.equal(f.calls.filter((c) => c.path === '/api/auth/accept').length, 1);
});
