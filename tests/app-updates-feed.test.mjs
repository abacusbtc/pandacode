import test from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';

/* What the app does with an update feed that does not answer (PAN-151).
 *
 * The dangerous behaviour was never "an update downloads" — it was "an update the person
 * did not ask for installs itself once nothing is running and they step away"
 * (`readyToRestart`). Repointing the feed at this fork makes a missing release harmless
 * only if a failed check produces no offer at all, so this drives the real module: the
 * Tauri updater plugin reaches the Rust side through `window.__TAURI_INTERNALS__.invoke`,
 * and stubbing that is the whole own-machinery. No network, no Tauri, and every command
 * the app attempted is recorded, which is what the assertions are about.
 *
 * The second test is the control: the same module, given a feed that answers and offers
 * nothing, must say so. Without it a broken updater that always failed would pass the
 * first test.
 *
 * The app's own imports are extensionless because the bundler resolves them; Node needs
 * the `.ts` spelled out, so the hook below is the one accommodation made to load it. */
registerHooks({
  resolve(specifier, context, next) {
    if (/^\.\.?\//.test(specifier) && !/\.[a-z]+$/.test(specifier)) return next(`${specifier}.ts`, context);
    return next(specifier, context);
  },
});

const calls = [];
let reply = () => { throw new Error('Could not fetch a valid release JSON from the remote: 404 Not Found'); };
// Defined rather than assigned: Node exposes a `localStorage` getter that warns when it
// has no file to store in, and reading it to test for presence is what warns.
Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: { getItem: () => null, setItem: () => {} } });
globalThis.addEventListener ??= () => {};
globalThis.window = {
  __TAURI_INTERNALS__: { invoke: async (command, args) => { calls.push({ command, args }); return reply(); } },
  // The launch check and the six-hour interval are registered through `window`; neither
  // fires here, so the tests drive `check()` themselves.
  setTimeout: () => 0,
  setInterval: () => 0,
  addEventListener: () => {},
  removeEventListener: () => {},
};

const { startAppUpdates, describeUpdate } = await import('../src/app-updates.ts');

/** A fresh updater on the same stubbed window; only `check()` and `restart()` are driven. */
function updater() {
  calls.length = 0;
  const noticed = [];
  return { updates: startAppUpdates({ busy: () => false, notify: text => noticed.push(text) }), noticed };
}

test('a feed that answers 404 offers no update, downloads nothing and installs nothing', async () => {
  const { updates, noticed } = updater();
  assert.equal(updates.supported, true, 'outside the browser preview the updater is supported');

  await updates.check();

  assert.equal(updates.state.kind, 'error');
  assert.match(updates.state.message, /404/, 'About says what happened instead of offering something');
  assert.match(describeUpdate(updates.state, true), /^Couldn’t check for updates: .*404/);
  assert.doesNotMatch(describeUpdate(updates.state, true), /is available|is ready/);

  // Asking the feed is the only thing a failed check may do.
  assert.deepEqual(calls.map(call => call.command), ['plugin:updater|check']);

  // A restart has nothing to install, so it must not fetch or install in the hope that an
  // earlier check left something behind.
  await updates.restart();
  assert.equal(updates.state.kind, 'error');
  assert.deepEqual(calls.map(call => call.command), ['plugin:updater|check']);
  assert.deepEqual(noticed, [], 'nothing happened that the person needs to be told to wait for');
});

test('a feed that answers with no release says the app is up to date', async () => {
  reply = () => null; // what the plugin returns when `latest.json` names nothing newer
  const { updates } = updater();

  await updates.check();

  assert.equal(updates.state.kind, 'current');
  assert.deepEqual(calls.map(call => call.command), ['plugin:updater|check']);
});
