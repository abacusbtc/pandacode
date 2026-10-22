import { expect, test, type Page } from '@playwright/test';
import { createServer } from 'node:http';

// The connect UI against a stub gateway that answers the same JSON-RPC envelope the
// real server does: POST /rpc {id, command, args} -> {id, ok, result} or {id, ok: false, error}.
// No events socket is opened here: nothing subscribes after the connection is made.

type Reply = unknown | ((args: any) => unknown);
type Replies = Record<string, Reply>;

type Stub = { url: string; calls: string[]; close: () => Promise<void> };

async function startGateway(replies: Replies): Promise<Stub> {
  const calls: string[] = [];
  const server = createServer((request, response) => {
    // The real gateway answers the browser's token-less preflight and names the
    // headers a client may send (src-tauri/src/gateway/server.rs, `cors`). Without
    // this the browser fails the fetch before /rpc is ever reached.
    const allow: Record<string, string> = {
      'Access-Control-Allow-Origin': request.headers.origin ?? '*',
      'Access-Control-Allow-Methods': 'GET, POST',
      'Access-Control-Allow-Headers': 'authorization, content-type',
      'Access-Control-Max-Age': '600',
      Vary: 'Origin',
    };
    if (request.method === 'OPTIONS') {
      response.writeHead(204, allow);
      response.end();
      return;
    }
    let body = '';
    request.on('data', chunk => { body += chunk; });
    request.on('end', () => {
      if (request.method !== 'POST' || !request.url?.startsWith('/rpc')) {
        response.writeHead(404, allow);
        response.end('not found');
        return;
      }
      const frame = JSON.parse(body || '{}');
      calls.push(frame.command);
      const reply = replies[frame.command];
      const footer = { ...allow, 'Content-Type': 'application/json' };
      if (reply === undefined) {
        response.writeHead(200, footer);
        response.end(JSON.stringify({ id: frame.id, ok: false, error: `unsupported_command: ${frame.command}` }));
        return;
      }
      const result = typeof reply === 'function' ? (reply as (args: unknown) => unknown)(frame.args) : reply;
      response.writeHead(200, footer);
      response.end(JSON.stringify({ id: frame.id, ok: true, result }));
    });
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = (server.address() as { port: number }).port;
  return { url: `http://127.0.0.1:${port}`, calls, close: () => new Promise<void>(resolve => {
    // fetch keeps its connections alive; closing them keeps teardown instant.
    server.closeAllConnections?.();
    server.close(() => resolve());
  }) };
}

// A seeded project and task, so the assertions can tell the gateway's data from the
// preview's fixtures (orbit, pandacode) and prove boot really went remote.
const PROJECT = { id: 'project-srv', name: 'srv-orbit', path: '/srv/repos/srv-orbit' };
const SESSION = { id: 'session-srv', projectId: PROJECT.id, title: 'Seeded from the server', provider: 'claude', model: 'sonnet', mode: 'ask', status: 'idle', createdAt: 1, updatedAt: 2, archived: false, worktreePath: null, resumeId: null, lastResponseAt: null };
// What Task 8's gateway serves (D4).
const M1 = ['add_project', 'list_projects', 'remove_project', 'list_sessions', 'create_session', 'get_messages', 'gateway_status'];
const seeded = (commands: string[] = M1): Replies => ({
  gateway_status: { commands, features: ['projects', 'sessions'] },
  list_projects: [PROJECT],
  list_sessions: [SESSION],
  get_messages: { messages: [], hasOlder: false },
});

const badge = (page: Page) => page.locator('#gateway-badge');

/**
 * entry.ts imports main.ts asynchronously, so a plain goto resolves before the
 * workspace and its preview hook exist. Waiting here is what keeps every assertion
 * about the app rather than about the load race.
 */
async function openApp(page: Page): Promise<void> {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).__connectGatewayForTest === 'function');
}

async function connect(page: Page, gateway: Stub): Promise<void> {
  await page.evaluate(([url]) => (window as any).__connectGatewayForTest(url, 'test-token'), [gateway.url]);
  await expect(badge(page)).toContainText('Connected to 127.0.0.1');
}

test('boot connects to a saved gateway before the first workspace fetch', async ({ page }) => {
  const gateway = await startGateway(seeded());
  try {
    await openApp(page);
    // The first connection saves the address, the same thing Settings → Connect does.
    await connect(page, gateway);
    await expect(page.locator('#navigation')).toContainText('Seeded from the server');

    // Relaunch: boot must read the saved gateway and route the first fetch through it.
    gateway.calls.length = 0;
    await openApp(page);
    await expect(badge(page)).toContainText('Connected to 127.0.0.1');
    await expect(page.locator('#navigation')).toContainText('srv-orbit');
    await expect(page.locator('#navigation')).toContainText('Seeded from the server');
    expect(gateway.calls).toEqual(expect.arrayContaining(['gateway_status', 'list_projects', 'list_sessions']));
    // The gateway serves no agent catalog, and asking would fail the whole load (D8).
    expect(gateway.calls).not.toContain('list_providers');

    // Absent from the command list: browser, VM, simulator and the quick launcher.
    // Open the inspector so its tabs have a box; unhide the platform-hidden ones so
    // the assertion is about the gateway's command list, not the host OS.
    await page.locator('[data-action="toggle-inspector"]').click();
    await page.locator('.inspector-tabs').waitFor({ state: 'visible' });
    await page.evaluate(() => {
      for (const action of ['tab-vm', 'tab-simulator']) document.querySelector(`[data-action="${action}"]`)!.removeAttribute('hidden');
    });
    await expect(page.locator('#browser-toggle')).toBeHidden();
    await expect(page.locator('.inspector-tabs [data-action="browser"]')).toBeHidden();
    await expect(page.locator('[data-action="tab-vm"]')).toBeHidden();
    await expect(page.locator('[data-action="tab-simulator"]')).toBeHidden();

    // A surface the gateway does serve is still there.
    await expect(page.locator('[data-action="add-project"]')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

test('keeps the surfaces a gateway advertises', async ({ page }) => {
  await openApp(page);
  // On a Mac these are visible by default; unhide them so the assertion is about the
  // command list rather than the host platform. Done before connecting, so the gate
  // is what would hide them.
  await page.evaluate(() => {
    for (const action of ['tab-vm', 'tab-simulator']) document.querySelector(`[data-action="${action}"]`)!.removeAttribute('hidden');
  });
  const gateway = await startGateway(seeded([...M1, 'vm_status', 'simulator_status', 'browser_open']));
  try {
    await connect(page, gateway);
    // The workspace reload behind the connection folds the inspector; open it again.
    await page.locator('[data-action="toggle-inspector"]').click();
    await page.locator('.inspector-tabs').waitFor({ state: 'visible' });
    await expect(page.locator('#browser-toggle')).toBeVisible();
    await expect(page.locator('.inspector-tabs [data-action="browser"]')).toBeVisible();
    await expect(page.locator('[data-action="tab-vm"]')).toBeVisible();
    await expect(page.locator('[data-action="tab-simulator"]')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

test('connects from Settings and disconnects back to the local transport', async ({ page }) => {
  // Two Settings round trips and a workspace reload each way.
  test.setTimeout(45_000);
  const gateway = await startGateway(seeded());
  try {
    await openApp(page);
    await page.locator('.sidebar-bottom [data-action="settings"]').click();
    const url = page.locator('#gateway-url');
    await url.fill(gateway.url);
    await page.locator('#gateway-token').fill('test-token');
    await page.locator('[data-action="gateway-connect"]').click();
    await expect(page.locator('#gateway-settings-status')).toContainText('Connected to 127.0.0.1');

    // Settings covers the workspace (visibility:hidden), so look at the header closed.
    await page.locator('[data-settings-close]').click();
    await expect(badge(page)).toContainText('Connected to 127.0.0.1');
    await expect(page.locator('#browser-toggle')).toBeHidden();

    await page.locator('.sidebar-bottom [data-action="settings"]').click();
    await page.locator('[data-action="gateway-disconnect"]').click();
    await expect(page.locator('#gateway-settings-status')).toContainText('Not connected');
    await page.locator('[data-settings-close]').click();

    await expect(badge(page)).toBeHidden();
    // The local transport answers again, so its surfaces come back.
    await expect(page.locator('#browser-toggle')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

test('an unreachable gateway is reported, not silently swapped for the local database', async ({ page }) => {
  await openApp(page);
  // Port 1 on loopback refuses connections immediately.
  await page.evaluate(() => (window as any).__connectGatewayForTest('http://127.0.0.1:1', 'test-token').catch(() => {}));
  await expect(badge(page)).toBeVisible();
  await expect(badge(page)).toContainText('Cannot reach');
});
