import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

/* The shipping configuration — `src-tauri/tauri.conf.json` — is a security control and a
 * distribution decision, and neither is visible from the browser preview, which is why two
 * defects lived here unnoticed:
 *
 *   PAN-150  `connect-src` named only IPC, so a packaged app could not `fetch` a remote
 *            gateway's `/rpc` or open its `/events` socket. The request died inside the
 *            app and Settings told the person "the gateway is not running", which was false.
 *   PAN-151  `plugins.updater.endpoints` pointed at the upstream repository with upstream's
 *            key, so an upstream release verified on this fork's build and could replace it.
 *
 * `PANDACODE_TAURI_CONF` points these assertions at a copy of another configuration, which
 * is how the negative proof runs them against the previous one.
 */
const PATH = process.env.PANDACODE_TAURI_CONF ?? fileURLToPath(new URL('../src-tauri/tauri.conf.json', import.meta.url));
const config = async () => JSON.parse(await readFile(PATH, 'utf8'));

/** The sources of one CSP directive, or `null` when the policy does not name it. */
function directive(policy, name) {
  for (const part of policy.split(';')) {
    const [head, ...sources] = part.trim().split(/\s+/);
    if (head === name) return sources;
  }
  return null;
}

/** The directives a policy names, in order. */
const names = policy => policy.split(';').map(part => part.trim().split(/\s+/)[0]).filter(Boolean);

test('a packaged app may reach a TLS gateway: connect-src allows https: and wss:', async () => {
  const sources = directive((await config()).app.security.csp, 'connect-src');
  assert.ok(sources, 'the policy names connect-src');
  assert.ok(sources.includes('https:'),
    "connect-src must allow https: or a packaged app cannot fetch a remote gateway's /rpc (PAN-150)");
  assert.ok(sources.includes('wss:'),
    "connect-src must allow wss: or a packaged app cannot open a gateway's /events socket (PAN-150)");
});

test('connect-src keeps IPC and loopback, and admits no other cleartext host', async () => {
  const sources = directive((await config()).app.security.csp, 'connect-src');
  for (const kept of ["'self'", 'ipc:', 'http://ipc.localhost', 'http://localhost:*', 'ws://localhost:*', 'http://127.0.0.1:*', 'ws://127.0.0.1:*']) {
    assert.ok(sources.includes(kept), `connect-src still needs ${kept} (IPC, or a gateway on this computer)`);
  }
  // A bare `http:`/`ws:` grants cleartext to every host on the wire, and the gateway holds a
  // credential that is a shell: TLS at a proxy is the documented deployment (docs/gateway.md).
  for (const cleartext of ['http:', 'ws:']) {
    assert.ok(!sources.includes(cleartext), `connect-src must not grant ${cleartext} to every host`);
  }
  for (const source of sources.filter(source => /^[a-z]+:\/\//.test(source))) {
    const { protocol, hostname } = new URL(source.replace(':*', ''));
    if (protocol === 'https:' || protocol === 'wss:') continue;
    assert.ok(['localhost', '127.0.0.1', 'ipc.localhost'].includes(hostname),
      `${source} is cleartext to a host that is not this computer`);
  }
});

test('nothing else in the policy loosens', async () => {
  const csp = (await config()).app.security.csp;
  // Pinned rather than spot-checked: a change to any of these is a deliberate edit to a
  // security control, so it should have to show up as a failing test and a reviewed diff.
  const pinned = {
    'default-src': ["'self'"],
    'script-src': ["'self'", "'wasm-unsafe-eval'"],
    'style-src': ["'self'", "'unsafe-inline'"],
    'img-src': ["'self'", 'data:', 'asset:', 'http://asset.localhost'],
    'font-src': ["'self'", 'data:'],
    'object-src': ["'none'"],
    'base-uri': ["'self'"],
    'frame-src': ["'none'"],
  };
  for (const [name, sources] of Object.entries(pinned)) assert.deepEqual(directive(csp, name), sources, `${name} changed`);
  assert.deepEqual(names(csp), ['default-src', 'script-src', 'style-src', 'img-src', 'connect-src', 'font-src', 'object-src', 'base-uri', 'frame-src'],
    'the policy names exactly these directives, so a new one cannot slip past this test');
});

test('the app does not switch the asset policy off instead of extending it', async () => {
  const security = (await config()).app.security;
  for (const key of Object.keys(security)) assert.ok(!/^dangerous/i.test(key), `${key} disables a protection rather than configuring it`);
});

test('the updater asks this fork for releases, never upstream', async () => {
  const endpoints = (await config()).plugins.updater.endpoints;
  assert.ok(Array.isArray(endpoints) && endpoints.length > 0, 'the updater names a feed');
  for (const endpoint of endpoints) {
    assert.doesNotMatch(endpoint, /aicodeking/,
      `a fork build must never accept the upstream feed (PAN-151): ${endpoint}`);
    assert.match(endpoint, /^https:\/\/github\.com\/abacusbtc\/pandacode\/releases\/latest\/download\/latest\.json$/,
      `the feed is this fork's latest release: ${endpoint}`);
  }
});

test('the updater keeps its Windows install mode, and its key is still upstream\'s', async () => {
  const updater = (await config()).plugins.updater;
  assert.equal(updater.windows.installMode, 'passive', 'an install nobody watches must not wait on a wizard');

  // PAN-151 leaves the key alone on purpose: rotating it needs a private key in a repo
  // secret, which is the owner's decision. The recorded key id makes that visible rather
  // than silent — today it is upstream's `378D4FF0CB8C4F81`, so an upstream-signed
  // `latest.json` still verifies if it is ever fetched, and one of ours would not until
  // the key is rotated. A rotation is expected to change this assertion and its comment.
  const comment = Buffer.from(updater.pubkey, 'base64').toString('utf8').split('\n')[0];
  assert.equal(comment, 'untrusted comment: minisign public key: 378D4FF0CB8C4F81',
    'the built-in update key changed: say whose key it is now, and what that means for this fork');
});
