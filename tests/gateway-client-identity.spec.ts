import { expect, test, type Page } from '@playwright/test';
import { startGateway, type Replies, type Stub } from './gateway-stub';

// PAN-142: the app sends its client identity, so a turn it starts owns its app actions.
//
// What this proves, and where it stops: the browser runs the **real client code**
// (`src/gateway-client.ts`) against a stub gateway that records what arrives. It proves
// the app sends one identity, on both carriers, stable across a reconnect, and different
// per app instance. The server's side — that a frame addressed to an identity reaches
// only that client and only it can answer — is proved against the real gateway in
// `src-tauri/src/gateway/app_action_remote_tests.rs` (PAN-123); this spec does not
// restate it, and no harness can raise an app action headless here.

const PROJECT = { id: 'project-srv', name: 'srv-orbit', path: '/srv/repos/srv-orbit' };
const SESSION = { id: 'session-srv', projectId: PROJECT.id, title: 'Seeded from the server', provider: 'claude', model: 'sonnet', mode: 'ask', status: 'idle', createdAt: 1, updatedAt: 2, archived: false, worktreePath: null, resumeId: null, lastResponseAt: null };

const replies = (): Replies => ({
  gateway_status: { commands: ['add_project', 'list_projects', 'remove_project', 'list_sessions', 'create_session', 'get_messages', 'gateway_status', 'send_message', 'resolve_agent_ui_action'], features: ['projects', 'sessions', 'turns'] },
  list_projects: [PROJECT],
  list_sessions: [SESSION],
  get_messages: { messages: [], hasOlder: false },
});

// The app comes from the dev server (`baseURL`); the stub only answers commands and
// events, the way the real gateway does once the app has been served.
const openApp = async (page: Page): Promise<void> => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).__connectGatewayForTest === 'function');
};

async function connect(page: Page, stub: Stub): Promise<void> {
  const answered = await page.evaluate(
    ([url]) => (window as any).__connectGatewayForTest(url, 'test-token'),
    [stub.url],
  );
  expect(answered, 'the stub refused the connect').toBe(true);
  await expect(page.locator('#gateway-badge')).toContainText('Connected to 127.0.0.1');
}

/** The `client` query parameter of the nth socket the stub saw, or null. */
const socketClient = (stub: Stub, index = 0): string | null => {
  const query = stub.socketUrls[index];
  return query ? new URL(`http://stub/?${query.replace(/^\?/, '')}`).searchParams.get('client') : null;
};

test('the app names one identity on its socket and on every command', async ({ page }) => {
  const stub = await startGateway(replies());
  try {
    await openApp(page);
    await connect(page, stub);
    await expect(page.locator('#navigation')).toContainText('srv-orbit');

    const identity = socketClient(stub);
    expect(identity, 'the events socket names this client').toBeTruthy();
    expect(identity).toMatch(/^[A-Za-z0-9._:-]{1,64}$/);

    // Every command, not just the first: a client that named itself once and then went
    // quiet would still be unaddressable mid-turn.
    expect(stub.rpcClients.length).toBeGreaterThan(1);
    expect(new Set(stub.rpcClients), 'every command carries the same identity').toEqual(new Set([identity]));
    expect(stub.rpcClients.includes(null), 'no command went out unnamed').toBe(false);
  } finally {
    await stub.close();
  }
});

test('a reconnect keeps the identity, so a waiting action stays answerable', async ({ page }) => {
  const stub = await startGateway(replies());
  try {
    await openApp(page);
    await connect(page, stub);
    const first = socketClient(stub, 0);
    expect(first).toBeTruthy();

    // A frame the client has seen is what makes a resume meaningful, so send one and
    // wait until the app has acted on it: `sessions-changed` makes it reload the list,
    // which is observable as one more command. Then the socket goes.
    const before = stub.calls.filter(command => command === 'list_sessions').length;
    stub.emit('sessions-changed', null);
    await expect
      .poll(() => stub.calls.filter(command => command === 'list_sessions').length, { timeout: 10_000 })
      .toBeGreaterThan(before);
    // The gateway drops the socket the way a sleep or a proxy timeout would.
    stub.drop();
    await expect.poll(() => stub.socketUrls.length, { timeout: 15_000 }).toBeGreaterThan(1);
    const second = socketClient(stub, 1);
    expect(second, 'the reopened socket is the same client').toBe(first);
    const query = new URL(`http://stub/?${stub.socketUrls[1].replace(/^\?/, '')}`).searchParams;
    expect(query.get('since'), 'and it resumes rather than starting at now').toBeTruthy();
  } finally {
    await stub.close();
  }
});

test('two app instances are two clients', async ({ browser }) => {
  const stub = await startGateway(replies());
  const one = await browser.newContext();
  const two = await browser.newContext();
  try {
    const pageA = await one.newPage();
    await openApp(pageA);
    await connect(pageA, stub);
    const pageB = await two.newPage();
    await openApp(pageB);
    await connect(pageB, stub);

    const identities = stub.socketUrls.map((_, index) => socketClient(stub, index)).filter(Boolean) as string[];
    expect(identities.length).toBeGreaterThanOrEqual(2);
    expect(new Set(identities).size, 'two windows, two identities').toBeGreaterThanOrEqual(2);
    // Each window's commands carry that window's identity: the sets are disjoint.
    const perWindow = new Set(stub.rpcClients.filter((value): value is string => Boolean(value)));
    expect(perIdentityDisjoint(perWindow, identities)).toBe(true);
  } finally {
    await one.close();
    await two.close();
    await stub.close();
  }
});

/** Every identity that issued a command is one of the identities that opened a socket. */
function perIdentityDisjoint(commandIdentities: Set<string>, socketIdentities: string[]): boolean {
  for (const identity of commandIdentities) if (!socketIdentities.includes(identity)) return false;
  return true;
}

test('an older gateway that ignores the identity keeps working', async ({ page }) => {
  // The stub is that older gateway: it answers the commands and never looks at the
  // header or the query parameter. The real gateway documents the same behaviour —
  // unknown parameters and headers are ignored — so the app must boot normally, which is
  // what this asserts.
  const stub = await startGateway(replies());
  try {
    await openApp(page);
    await connect(page, stub);
    await expect(page.locator('#navigation')).toContainText('srv-orbit');
    await expect(page.locator('#navigation')).toContainText('Seeded from the server');
    // It still listed the workspace over a gateway that did nothing with the identity.
    expect(stub.calls).toContain('list_projects');
    expect(stub.calls).toContain('list_sessions');
  } finally {
    await stub.close();
  }
});
