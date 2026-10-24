// The rule behind remote mode: a connected gateway is asked only for the commands its
// own `gateway_status` answer named. The DOM side (which surface each gated command
// hides, and the attribute that hides it) is proved against a real gateway in
// tests/gateway-e2e.spec.ts; this pins the rule itself.
import test from 'node:test';
import assert from 'node:assert/strict';
import { GATED, featureServed, serves } from '../src/gateway-gating.ts';

// The M1 table in src-tauri/src/gateway/dispatch.rs. A name added there is not gated
// here until someone says which surface it makes real, which is the point of the list.
const M1 = ['add_project', 'list_projects', 'remove_project', 'list_sessions', 'create_session', 'get_messages', 'gateway_list_dirs', 'gateway_status'];

test('the local transports serve everything, including commands no gateway has', () => {
  assert.equal(serves(null, 'git_status'), true);
  assert.equal(serves(null, 'anything_at_all'), true);
});

test('a gateway serves exactly what it answered with', () => {
  for (const command of M1) assert.equal(serves(M1, command), true, command);
  for (const command of ['git_status', 'list_files', 'list_subagents', 'jev_list_runs', 'github_status', 'get_run_script', 'project_icon']) {
    assert.equal(serves(M1, command), false, command);
  }
});

test('a gateway that named no commands serves none, rather than falling back to everything', () => {
  // `attachGateway` paints with [] before the status reply arrives: a half-connected
  // window must not claim a surface it has not been told about.
  assert.equal(serves([], 'list_projects'), false);
});

test('a capability is advertised by the gateway, not assumed from the command list', () => {
  const turns = ['projects', 'sessions', 'turns'];
  assert.equal(featureServed(turns, 'turns'), true);
  // A gateway that predates the capability -- the M1/M2 shape before task 6 -- cannot
  // drive a turn, and telling it apart is the whole reason the name exists.
  assert.equal(featureServed(['projects', 'sessions'], 'turns'), false);
  // Local transports have every capability, and a half-connected window has none.
  assert.equal(featureServed(null, 'turns'), true);
  assert.equal(featureServed([], 'turns'), false);
});

test('every gated surface names the command that would make it real, and a selector to hide', () => {
  const commands = new Set();
  for (const entry of GATED) {
    assert.match(entry.command, /^[a-z][a-z0-9_]*$/, entry.command);
    assert.ok(entry.selector.trim().length > 0, `no selector for ${entry.command}`);
    assert.equal(commands.has(entry.command), false, `${entry.command} is gated twice`);
    commands.add(entry.command);
  }
});

test('the surfaces this change gated are gated on the command that decides them', () => {
  const selector = command => GATED.find(entry => entry.command === command)?.selector;
  // No upload route on the gateway: the .vsix file picker is the row that goes.
  assert.equal(selector('theme_import'), '[data-colors="import"]');
  // A gateway has no `gh` of its own and no pandacode-run.sh to read.
  assert.equal(selector('github_status'), '[data-action="github-view"]');
  assert.equal(selector('get_run_script'), '[data-action="run-project"]');
});
