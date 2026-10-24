// The remote turn surface, by effect, against a stub gateway (tests/gateway-stub.ts).
//
// What is proved here is what a person sitting at another machine gets: a stream whose
// numbers mean a hole is repaired rather than drawn, a reconnect that resumes instead of
// starting at "now", approvals answered over the same transport, a denial that stays
// visible after the window was away, an uploaded file the message names by id, a
// terminal that asks for its scrollback and says why it closed — and a seam that refuses
// in words when the gateway does not serve a turn at all.

import { expect, test, type Page } from '@playwright/test';
import { startGateway, type Replies, type Stub } from './gateway-stub';

const PROJECT = { id: 'project-srv', name: 'srv-orbit', path: '/srv/repos/srv-orbit' };
const SESSION = { id: 'session-srv', projectId: PROJECT.id, title: 'Seeded from the server', provider: 'claude', model: 'sonnet', mode: 'ask', status: 'idle', createdAt: 1, updatedAt: 2, archived: false, worktreePath: null, resumeId: null, lastResponseAt: null };

/** What PAN-107's gateway serves: the M1 commands plus the turn commands, under `turns`. */
const TURNS = [
  'add_project', 'list_projects', 'remove_project', 'list_sessions', 'create_session',
  'get_messages', 'gateway_status',
  'send_message', 'start_task', 'stop_session', 'respond_approval', 'respond_user_input', 'steer_task',
  'queue_list', 'queue_add', 'queue_update', 'queue_remove', 'queue_reorder', 'queue_take', 'queue_restore',
  'gateway_live_tasks',
];
/** PAN-110 adds the six terminal commands and the capability a client gates on. */
const TERMINAL = [...TURNS, 'start_terminal', 'write_terminal', 'resize_terminal', 'close_terminal', 'get_terminal_activity', 'get_terminal_preferences'];
/** M1, for the honest old-gateway case. */
const M1 = ['add_project', 'list_projects', 'remove_project', 'list_sessions', 'create_session', 'get_messages', 'gateway_status'];

const features = (...names: string[]) => ['projects', 'sessions', ...names];
const page_ = (...rows: unknown[]) => ({ messages: rows, hasOlder: false });
const userRow = { id: 1, sessionId: SESSION.id, role: 'user', content: 'Build the thing.', createdAt: 10 };
const agentRow = (content: string, id = 2) => ({ id, sessionId: SESSION.id, role: 'assistant', content, createdAt: 20 });

const seeded = (commands: string[] = TURNS, capability = features('turns'), extra: Replies = {}): Replies => ({
  gateway_status: { commands, features: capability },
  list_projects: [PROJECT],
  list_sessions: [SESSION],
  get_messages: page_(userRow),
  queue_list: [],
  gateway_live_tasks: [],
  ...extra,
});

const badge = (page: Page) => page.locator('#gateway-badge');
/** entry.ts imports main.ts asynchronously; the hook exists only once it has. */
async function openApp(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).__connectGatewayForTest === 'function');
}
const waitForBoot = async (page: Page) => expect(page.getByRole('region', { name: 'orbit tasks', exact: true })).toBeVisible();
async function connect(page: Page, gateway: Stub): Promise<void> {
  await page.evaluate(([url]) => (window as any).__connectGatewayForTest(url, 'test-token'), [gateway.url]);
  await expect(badge(page)).toContainText('Connected to 127.0.0.1');
}

// ---- Stream and reconnect -----------------------------------------------------------

test('a hole is announced, repaired from the transcript, and a reconnect resumes after it', async ({ page }) => {
  test.setTimeout(45_000);
  let saved = page_(userRow);
  let live: { sessionId: string }[] = [];
  const gateway = await startGateway(seeded(TURNS, features('turns'), {
    get_messages: () => saved,
    gateway_live_tasks: () => live,
  }));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await expect(page.locator('#transcript')).toContainText('Build the thing.');

    // A turn opens its stream on the first frame and streams a partial answer.
    live = [{ sessionId: SESSION.id }];
    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'status', text: 'Working…' });
    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'text', text: 'Half an answer' });
    await expect(page.locator('.live-message')).toContainText('Half an answer');

    // The ring drops the rest of the turn — including its `done` — and says so. In the
    // meantime the gateway saved the whole response, so the transcript is the authority.
    saved = page_(userRow, agentRow('Everything the agent said.'));
    live = [];
    gateway.announceHole(3, 9);

    // No fragment the client can no longer complete, no duplicate line, and a turn that
    // ends because the gateway says it is no longer live rather than waiting for a
    // `done` frame that fell in the hole.
    await expect(page.locator('#transcript')).toContainText('Everything the agent said.');
    await expect(page.locator('#transcript')).not.toContainText('Half an answer');
    expect(((await page.locator('#transcript').innerText()).match(/Everything the agent said\./g) ?? []).length).toBe(1);

    // A dropped socket resumes after the last frame it saw, not at "now".
    const before = gateway.socketUrls.length;
    gateway.drop();
    await expect.poll(() => gateway.socketUrls.length, { message: 'the client reopened its event socket' }).toBeGreaterThan(before);
    expect(gateway.socketUrls[gateway.socketUrls.length - 1]).toContain('since=9');

    // And what arrives next is drawn once: the replay carried no frame the client had
    // already rendered, so nothing is duplicated and nothing is missing.
    live = [{ sessionId: SESSION.id }];
    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'text', text: 'Only this is new.' });
    await expect(page.locator('.live-message')).toContainText('Only this is new.');
    expect(((await page.locator('#transcript').innerText()).match(/Only this is new\./g) ?? []).length).toBe(1);
  } finally {
    await gateway.close();
  }
});

// ---- Approvals ---------------------------------------------------------------------

test('an approval is rendered from the gateway and answered once, with its request id', async ({ page }) => {
  const answered: Record<string, unknown>[] = [];
  const gateway = await startGateway(seeded(TURNS, features('turns'), {
    respond_approval: (args: Record<string, unknown>) => { answered.push(args); return null; },
  }));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);

    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'approval', requestId: 'req-1', text: 'Run `rm -rf build`?' });
    const bar = page.locator('#approval-bar');
    // Approval text is Markdown everywhere; the one-line summary drops the code ticks.
    await expect(bar).toContainText('Run rm -rf build?');

    await bar.locator('[data-action="approval-allow"]').click();
    await expect.poll(() => answered.length).toBe(1);
    expect(answered[0]).toEqual({ sessionId: SESSION.id, requestId: 'req-1', approved: true });
    // Exactly one answer, and the prompt is gone once the turn has it.
    expect(answered).toHaveLength(1);
    await expect(bar).toHaveText('');
  } finally {
    await gateway.close();
  }
});

test('a question auto-denied while nobody was connected stays visible, with its reason', async ({ page }) => {
  test.setTimeout(45_000);
  const denial = 'No client was connected for 1 h; the request was denied.';
  let saved = page_(userRow);
  const gateway = await startGateway(seeded(TURNS, features('turns'), { get_messages: () => saved }));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);

    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'approval', requestId: 'req-2', text: 'Push the branch?' });
    await expect(page.locator('#approval-bar')).toContainText('Push the branch?');

    // The gateway denies it after its TTL (PAN-106) and writes the reason down as a row.
    saved = page_(userRow, { id: 3, sessionId: SESSION.id, role: 'approval_denied', content: `${denial}\n\nRequest: req-2`, createdAt: 30 });
    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'approval_denied', requestId: 'req-2', text: denial });

    // A stated outcome in the live view, not a prompt that vanished...
    await expect(page.locator('.message-denied')).toContainText(denial);
    await expect(page.locator('#approval-bar')).not.toContainText('Push the branch?');

    // ...and the durable half: the person who comes back later reads the same sentence,
    // which came from the saved row and not from any frame this window was awake for.
    // Boot reads the saved gateway, so this reload is a fresh window on the same data.
    await openApp(page);
    await expect(badge(page)).toContainText('Connected to 127.0.0.1');
    await expect(page.locator('#navigation')).toContainText('Seeded from the server');
    await expect(page.locator('.message-denied')).toContainText(denial);
    await expect(page.locator('#approval-bar')).not.toContainText('Push the branch?');
  } finally {
    await gateway.close();
  }
});

// ---- Uploads -----------------------------------------------------------------------

/** The composer's attach menu, then the browser's own file picker. */
async function attachFile(page: Page, name: string, mimeType: string, body: string): Promise<void> {
  const chooser = page.waitForEvent('filechooser');
  await page.getByRole('button', { name: 'Attach files for context' }).click();
  await page.getByRole('dialog', { name: 'Attach', exact: true }).getByRole('menuitem', { name: /Attach files…/ }).click();
  await (await chooser).setFiles({ name, mimeType, buffer: Buffer.from(body) });
}

test('a file is uploaded to the gateway and the message names it by id', async ({ page }) => {
  test.setTimeout(45_000);
  const gateway = await startGateway(
    seeded(TURNS, features('turns'), { send_message: () => null }),
    // Held open so the chip's uploading state is observable, as a slow link would show.
    { upload: async (name, kind, body) => {
      await new Promise(resolve => setTimeout(resolve, 400));
      return { status: 200, body: { id: 'upload-1', name, kind, bytes: body.length, sha256: 'stub', createdAt: Date.now() } };
    } },
  );
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);

    await attachFile(page, 'notes.md', 'text/markdown', '# Notes\nhello\n');
    // Progress, then the receipt: the composer says where the file is, not just that it
    // is attached, and the temporary chip is replaced by the one the gateway named.
    await expect(page.locator('#attachments')).toContainText('Uploading');
    await expect(page.locator('#attachments')).toContainText('notes.md');
    await expect(page.locator('#attachments')).toContainText('on the gateway');
    await expect(page.locator('#attachments')).not.toContainText('Uploading');

    await page.locator('#prompt').fill('Summarise this');
    await page.locator('#prompt').press('Enter');
    await expect.poll(() => gateway.requests.filter(request => request.command === 'send_message').length).toBe(1);
    const send = gateway.requests.find(request => request.command === 'send_message')!;
    // The bytes went to the gateway once, and the message carries the id rather than a
    // path on this computer.
    expect(gateway.uploads).toEqual([{ name: 'notes.md', kind: 'document', bytes: Buffer.byteLength('# Notes\nhello\n') }]);
    expect(send.args.uploadIds).toEqual(['upload-1']);
    expect(send.args.text).toContain('Summarise this');
  } finally {
    await gateway.close();
  }
});

test('a refused upload surfaces the gateway’s own reason and attaches nothing', async ({ page }) => {
  const gateway = await startGateway(
    seeded(TURNS, features('turns')),
    { upload: () => ({ status: 413, body: { error: 'upload_too_large: notes.md is 250 MiB, the limit is 200 MiB' } }) },
  );
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await attachFile(page, 'notes.md', 'text/markdown', '# Notes');
    await expect(page.locator('#toast')).toContainText('upload_too_large');
    // A file the gateway does not have is not left looking attached.
    await expect(page.locator('#attachments')).not.toContainText('notes.md');
  } finally {
    await gateway.close();
  }
});

// ---- Stop and queue ----------------------------------------------------------------

test('stop and the queue act on the gateway it is talking to', async ({ page }) => {
  const gateway = await startGateway(seeded(TURNS, features('turns'), {
    stop_session: () => null,
    queue_list: (args: Record<string, unknown>) => args?.sessionId ? [{ id: 'queued-1', sessionId: SESSION.id, position: 0, text: 'And then the tests', attachments: [], createdAt: 5, updatedAt: 5 }] : [],
    queue_take: () => { throw new Error('queue_empty'); },
  }));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);

    // A running turn offers Stop, and Stop is the gateway's own command.
    gateway.emit('agent-event', { sessionId: SESSION.id, kind: 'status', text: 'Working…' });
    const stop = page.locator('#composer [data-action="stop"]');
    await expect(stop).toBeVisible();
    await stop.click();
    await expect.poll(() => gateway.requests.filter(request => request.command === 'stop_session').length).toBe(1);
    expect(gateway.requests.find(request => request.command === 'stop_session')!.args).toEqual({ sessionId: SESSION.id });

    // The queue follows the gateway's `queue-changed` frame: no navigation needed.
    gateway.emit('queue-changed', { sessionId: SESSION.id });
    await expect(page.locator('.queue-panel')).toContainText('And then the tests');
  } finally {
    await gateway.close();
  }
});

// ---- Terminal ----------------------------------------------------------------------

/** A gateway that serves the terminal, numbering its PTYs from one per stub. */
function terminalReplies(): Replies {
  let started = 0;
  return seeded(TERMINAL, features('turns', 'terminal'), {
    start_terminal: () => `term-${++started}`,
    write_terminal: () => null,
    resize_terminal: () => null,
    close_terminal: () => null,
    get_terminal_preferences: () => ({}),
    get_terminal_activity: () => ({ running: false, processNames: [], uncertain: false }),
  });
}

test('a remote terminal asks for scrollback, restarts on a transport change, and says why it closed', async ({ page }) => {
  test.setTimeout(60_000);
  const gateway = await startGateway(terminalReplies());
  const second = await startGateway(terminalReplies());
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await page.locator('#terminal-toggle').click();

    // The filtered socket with since=0 is two things at once: the scrollback a console
    // that has just opened has never seen, and the gateway's record that it is watched.
    await expect.poll(() => gateway.socketUrls.filter(url => url.includes('terminal=term-1')).length).toBe(1);
    expect(gateway.socketUrls.find(url => url.includes('terminal=term-1'))).toContain('since=0');
    // The shell is told the size the pane settled on, not the size it had when it started.
    await expect.poll(() => gateway.calls.filter(command => command === 'resize_terminal').length).toBeGreaterThan(0);

    gateway.emitTerminal('term-1', 'terminal-event', { terminalId: 'term-1', data: 'hello from the shell\r\n' });
    await expect(page.locator('.xterm-rows')).toContainText('hello from the shell');

    // The ring could not reach back far enough: a shell cannot be refetched, so the
    // gateway says so and the pane clears and says it too, instead of splicing.
    gateway.emitTerminal('term-1', 'terminal-reset', { fromSeq: 1, toSeq: 4 });
    await expect(page.locator('.xterm-rows')).toContainText('reset');

    // A transport change means this PTY is on the other machine: restart here and say so.
    await page.evaluate(([url]) => (window as any).__connectGatewayForTest(url, 'test-token'), [second.url]);
    await expect.poll(() => second.calls.filter(command => command === 'start_terminal').length).toBe(1);
    await expect(page.locator('.xterm-rows')).toContainText('Moved to the gateway');
    await expect.poll(() => second.socketUrls.filter(url => url.includes('terminal=term-1')).length).toBe(1);

    // An idle close carries its reason, so the pane is not just a dead tab.
    second.emitTerminal('term-1', 'terminal-event', { terminalId: 'term-1', data: '', exit: true, reason: 'idle' });
    await expect(page.locator('.xterm-rows')).toContainText('idle');
    await expect(page.locator('#terminal-toggle')).toBeVisible();
  } finally {
    await second.close();
    await gateway.close();
  }
});

// ---- Capability gating -------------------------------------------------------------

test('a gateway without `turns` is not offered a composer, a terminal, or usage', async ({ page }) => {
  const gateway = await startGateway(seeded(M1, features()));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);

    // Not a disabled prompt that fails on Enter: no prompt, and the reason on screen.
    await expect(page.locator('#prompt')).toHaveCount(0);
    await expect(page.locator('#composer-area')).toContainText('cannot run agent turns');
    // The surfaces whose commands need this machine's own window stay hidden too.
    await expect(page.locator('#agent-usage-button')).toBeHidden();
    await expect(page.locator('#terminal-toggle')).toBeHidden();
  } finally {
    await gateway.close();
  }
});

test('a gateway advertising `turns` and its commands is offered the whole turn surface', async ({ page }) => {
  const gateway = await startGateway(seeded([...TERMINAL, 'agent_usage'], features('turns', 'terminal')));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await expect(page.locator('#prompt')).toBeVisible();
    await expect(page.locator('#agent-usage-button')).toBeVisible();
    await expect(page.locator('#terminal-toggle')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

// ---- The old gateway ---------------------------------------------------------------

test('a turn command an old gateway refuses is reported in the person’s terms', async ({ page }) => {
  const gateway = await startGateway(seeded(TURNS, features('turns'), {
    // A proxy in front of an older gateway, or a capability list that ran ahead of the
    // command table: the send is refused on the wire and must not fail silently.
    send_message: () => { throw new Error('unsupported_command: send_message'); },
  }));
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await page.locator('#prompt').fill('Hello there');
    await page.locator('#prompt').press('Enter');
    await expect(page.locator('#toast')).toContainText('does not serve send_message');
    // The message is not lost: the draft comes back for the next attempt.
    await expect(page.locator('#prompt')).toHaveValue('Hello there');
  } finally {
    await gateway.close();
  }
});
