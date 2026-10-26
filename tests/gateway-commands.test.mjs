import test from 'node:test';
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { DYNAMIC_CALLS, REMOTE_COMMANDS, SERVED, TRANSPORTS } from '../src/gateway-commands.ts';

/* PAN-160: the served-command gap, closed by assertion rather than by tripping over it.
 *
 * The owner found `configure_session` missing by selecting a model and getting
 * `unsupported_command` — the third such surprise in two days (PAN-124, PAN-125, PAN-159),
 * each discovered by a person using the app. The measured shape: the frontend can call 207
 * literal commands plus three that are dispatched through a variable, the gateway serves 35,
 * and nothing said which of the rest were deliberately unavailable.
 *
 * This file reads the sources, so the classification cannot go stale in either direction:
 * a new call site with no entry fails, and a command the gateway starts serving while the
 * table still calls it unavailable fails too.
 */

const root = fileURLToPath(new URL('..', import.meta.url));
const sources = async () => {
  // The classification table itself is documentation: it names `call(...)` in prose (the
  // alias it cannot see, for one), so it is not a source of call sites.
  const names = (await readdir(`${root}src`)).filter(name => name.endsWith('.ts') && name !== 'gateway-commands.ts').sort();
  return Promise.all(names.map(async name => ({ name: `src/${name}`, text: await readFile(`${root}src/${name}`, 'utf8') })));
};

/** Every command named as a string literal at a `call(...)` site. */
function literalSites(files) {
  const found = new Set();
  for (const { text } of files) {
    for (const match of text.matchAll(/(?:\.)?\bcall(?:<[^>]*>)?\('([a-z][a-z0-9_]*)'/g)) found.add(match[1]);
  }
  return found;
}

/**
 * Call sites whose first argument is a variable. Declarations (`call<T>(command: string)`)
 * and `Function.prototype.call` are not call sites and are excluded by shape: an argument
 * is a variable when an identifier is followed by `,` or `)`, not by `:`.
 */
function dynamicSites(files) {
  const found = [];
  for (const { name, text } of files) {
    for (const match of text.matchAll(/(?:\.)?\bcall(?:<[^>]*>)?\(/g)) {
      const rest = text.slice(match.index + match[0].length);
      if (rest.startsWith("'") || rest.startsWith('"')) continue;
      if (!/^[A-Za-z_$][\w$]*\s*[,)]/.test(rest)) continue;
      if (rest.startsWith('this,') || rest.startsWith('this)')) continue; // Function.prototype.call
      found.push(`${name}:${text.slice(0, match.index).split('\n').length}`);
    }
  }
  return found;
}

/** The gateway's own command list, read from the Rust table it dispatches from. */
async function gatewayCommands() {
  const rust = await readFile(`${root}src-tauri/src/gateway/dispatch.rs`, 'utf8');
  const block = rust.split('pub(crate) const COMMANDS')[1].split('];')[0];
  return [...block.matchAll(/"([a-z0-9_]+)"/g)].map(match => match[1]);
}

const unclassified = (sites) => [...sites].filter(command => !REMOTE_COMMANDS[command]).sort();

test('every command the frontend can call has exactly one state', async () => {
  const sites = literalSites(await sources());
  assert.ok(sites.size > 150, `the scan found the app's call sites (${sites.size})`);
  assert.deepEqual(unclassified(sites), [], 'unclassified call sites: add each to REMOTE_COMMANDS with a reason');
  for (const [command, entry] of Object.entries(REMOTE_COMMANDS)) {
    assert.ok(['served', 'gated', 'unavailable'].includes(entry.state), `${command}: unknown state ${entry.state}`);
  }
});

test('a call site that names its command in a variable is declared', async () => {
  const files = await sources();
  const dynamic = dynamicSites(files).filter(site => !site.includes('src/bridge.ts') && !site.includes('src/dom.ts') && !site.includes('src/motion.ts'));
  // bridge.ts forwards to the transport, dom.ts and motion.ts call Function.prototype.call.
  const declared = new Set(DYNAMIC_CALLS.map(entry => entry.site.split(':')[0]));
  const undeclared = dynamic.filter(site => !declared.has(site.split(':')[0]));
  assert.deepEqual(undeclared, [], 'dynamic call sites must be declared in DYNAMIC_CALLS with the commands they pass');
  for (const entry of DYNAMIC_CALLS) {
    assert.ok(entry.why.trim().length > 20, `${entry.site}: a declaration needs a reason someone could disagree with`);
    assert.deepEqual(unclassified(entry.commands), [], `${entry.site}: its commands must be classified`);
  }
});

test('the table says served exactly when the gateway serves it', async () => {
  const served = await gatewayCommands();
  assert.deepEqual([...SERVED].sort(), served.sort(),
    'SERVED must equal the gateway\'s COMMANDS: a command added there must be marked here, one removed must be reclassified');
  for (const command of served) {
    assert.equal(REMOTE_COMMANDS[command]?.state, 'served', `${command} is served by the gateway but not marked so`);
    assert.equal(REMOTE_COMMANDS[command].reason, null, `${command} is served, so it carries no refusal`);
  }
});

test('every state that refuses a command says why, in a sentence', async () => {
  for (const [command, entry] of Object.entries(REMOTE_COMMANDS)) {
    if (entry.state === 'served') continue;
    assert.equal(typeof entry.reason, 'string', `${command}: ${entry.state} with no reason`);
    assert.ok(entry.reason.trim().length > 40, `${command}: the reason is a marker, not an argument (${entry.reason})`);
    assert.match(entry.reason, /\.$/, `${command}: the reason is not a sentence (${entry.reason})`);
    assert.doesNotMatch(entry.reason, /\bTODO\b|\bFIXME\b|\bTBD\b/i, `${command}: placeholder reason`);
  }
});

test('the DOM gate rows are classified, so the two tables cannot disagree', async () => {
  const gating = await readFile(`${root}src/gateway-gating.ts`, 'utf8');
  const rows = [...gating.split('export const GATED')[1].split('];')[0].matchAll(/{ command: '([a-z_]+)'/g)].map(match => match[1]);
  assert.ok(rows.length >= 10, 'the GATED rows were found');
  for (const command of rows) {
    const entry = REMOTE_COMMANDS[command];
    assert.ok(entry, `${command} is hidden by a GATED row but has no state`);
    // A row may describe an older gateway for a command this build serves (start_terminal).
    assert.ok(['served', 'gated'].includes(entry.state), `${command} is hidden by a GATED row but classified ${entry.state}`);
  }
});

/**
 * The transports, discovered rather than named (PAN-160's weak joint): every function that
 * reaches Tauri's `invoke` is an IPC surface, found by reading the sources. `preview.ts` and
 * `preview-bots.ts` are the browser preview's own stand-in for `invoke` — test plumbing, not a
 * transport the app routes through — and are the only exclusions.
 */
async function discoveredTransports() {
  const files = await sources();
  const found = new Set();
  for (const { name, text } of files) {
    if (name === 'src/preview.ts' || name === 'src/preview-bots.ts') continue;
    for (const match of text.matchAll(/\.invoke(?:<[^>]*>)?\(/g)) {
      const before = text.slice(0, match.index);
      const pattern = /(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)|(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:<[^>]*>\s*)?\(/g;
      let enclosing = null;
      for (const declaration of before.matchAll(pattern)) enclosing = declaration[1] ?? declaration[2];
      if (enclosing) found.add(enclosing);
    }
  }
  return found;
}

test('a transport is discovered from the code, so a new wrapper cannot go unscanned', async () => {
  const discovered = [...await discoveredTransports()].sort();
  const registered = TRANSPORTS.filter(entry => entry.kind === 'ipc').map(entry => entry.name).sort();
  assert.deepEqual(discovered, registered,
    'a function that reaches invoke must be registered in TRANSPORTS — otherwise its commands are never scanned, which certifies a completeness that is false');
  const routers = TRANSPORTS.filter(entry => entry.kind === 'router').map(entry => entry.name);
  assert.deepEqual(routers, ['call'], 'the router is the one place that chooses between this computer and a gateway');
  for (const entry of TRANSPORTS) assert.ok(entry.why.trim().length > 40, `${entry.name}: needs a reason, not a label`);
});

test('every command on a local transport is declared, and none pretends to be gateway-served', async () => {
  const files = await sources();
  const literals = transport => {
    const found = new Set();
    for (const { text } of files) for (const match of text.matchAll(new RegExp(`${transport}(?:<[^>]*>)?\\('([a-z][a-z0-9_]*)'`, 'g'))) found.add(match[1]);
    return found;
  };
  for (const entry of TRANSPORTS.filter(transport => !transport.routes && transport.kind === 'ipc')) {
    assert.deepEqual([...literals(entry.name)].sort(), [...entry.commands].sort(),
      `${entry.name}: its declared commands must equal the ones the sources call`);
  }
  // callShell is answered on this computer in every mode, so a gateway can never serve one.
  const shell = new Set(TRANSPORTS.find(entry => entry.name === 'callShell').commands);
  for (const command of shell) assert.notEqual(REMOTE_COMMANDS[command]?.state, 'served', `${command} is local-only by construction`);
  console.log(`\n  transports: ${TRANSPORTS.map(entry => `${entry.name}(${entry.kind}${entry.routes ? ', routes' : ''}) ${entry.commands.length}`).join(', ')}\n`);
});

test('the table has no entry for a command nobody can call', async () => {
  const files = await sources();
  const gating = await readFile(`${root}src/gateway-gating.ts`, 'utf8');
  const gateRows = [...gating.split('export const GATED')[1].split('];')[0].matchAll(/{ command: '([a-z_]+)'/g)].map(match => match[1]);
  const callable = new Set([...literalSites(files), ...DYNAMIC_CALLS.flatMap(entry => entry.commands), ...SERVED, ...gateRows, ...TRANSPORTS.flatMap(entry => entry.commands)]);
  const dead = Object.keys(REMOTE_COMMANDS).filter(command => !callable.has(command)).sort();
  assert.deepEqual(dead, [], 'a state for a command that is neither called nor served is a typo or a leftover');
});

test('the gap is counted, not just described', async () => {
  // The reason that says "not served yet" is the only one that is a TODO in disguise, so it
  // is counted here: growth is a decision someone makes, not something that happens.
  const notServed = Object.entries(REMOTE_COMMANDS)
    .filter(([, entry]) => entry.state === 'unavailable' && entry.reason.includes('does not serve this command yet'))
    .map(([command]) => command)
    .sort();
  assert.ok(notServed.length > 0);
  assert.ok(notServed.length <= 17, `the not-yet-served list grew to ${notServed.length}: ${notServed.join(', ')}`);
  console.log(`\n  not served yet (${notServed.length}): ${notServed.join(', ')}\n`);
});
