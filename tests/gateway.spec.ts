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
    let body = '';
    request.on('data', chunk => { body += chunk; });
    request.on('end', () => {
      if (request.method !== 'POST' || !request.url?.startsWith('/rpc')) {
        response.statusCode = 404; response.end('not found'); return;
      }
      const frame = JSON.parse(body || '{}');
      calls.push(frame.command);
      response.setHeader('Content-Type', 'application/json');
      if (!(frame.command in replies)) {
        response.end(JSON.stringify({ id: frame.id, ok: false, error: `unsupported_command: ${frame.command}` }));
        return;
      }
      const reply = replies[frame.command];
      const result = typeof reply === 'function' ? (reply as (args: unknown) => unknown)(frame.args) : reply;
      response.end(JSON.stringify({ id: frame.id, ok: true, result }));
    });
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = (server.address() as { port: number }).port;
  return { url: `http://127.0.0.1:${port}`, calls, close: () => new Promise<void>(resolve => server.close(() => resolve())) };
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

async function connect(page: Page, gateway: Stub): Promise<void> {
  await page.evaluate(([url]) => (window as any).__connectGatewayForTest(url, 'test-token'), [gateway.url]);
  await expect(badge(page)).toContainText('Connected to 127.0.0.1');
}

test('connects to a gateway on boot and hides the surfaces it does not serve', async ({ page }) => {
  const gateway = await startGateway(seeded());
  try {
    await page.goto('/');
    await expect(badge(page)).toBeHidden();
    await connect(page, gateway);

    // Boot fetched this workspace from the gateway, not the preview's fixtures.
    await expect(page.locator('#navigation')).toContainText('srv-orbit');
    expect(gateway.calls).toEqual(expect.arrayContaining(['gateway_status', 'list_projects', 'list_sessions']));

    // Absent from the command list: browser, VM, simulator and the quick launcher.
    await expect(page.locator('#browser-toggle')).toBeHidden();
    await expect(page.locator('[data-action="browser"]')).toBeHidden();
    await expect(page.locator('[data-action="tab-vm"]')).toBeHidden();
    await expect(page.locator('[data-action="tab-simulator"]')).toBeHidden();

    // A surface the gateway does serve is still there.
    await expect(page.locator('#add-project')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

test('keeps the surfaces a gateway advertises', async ({ page }) => {
  await page.goto('/');
  // On a Mac these are visible by default; unhide them so the assertion is about the
  // command list rather than the host platform.
  await page.evaluate(() => {
    for (const action of ['tab-vm', 'tab-simulator', 'browser']) document.querySelector(`[data-action="${action}"]`)!.removeAttribute('hidden');
  });
  const gateway = await startGateway(seeded([...M1, 'vm_status', 'simulator_status', 'browser_open']));
  try {
    await connect(page, gateway);
    await expect(page.locator('#browser-toggle')).toBeVisible();
    await expect(page.locator('[data-action="browser"]')).toBeVisible();
    await expect(page.locator('[data-action="tab-vm"]')).toBeVisible();
    await expect(page.locator('[data-action="tab-simulator"]')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

test('connects from Settings and disconnects back to the local transport', async ({ page }) => {
  const gateway = await startGateway(seeded());
  try {
    await page.goto('/');
    await page.locator('.sidebar-bottom [data-action="settings"]').click();
    const url = page.locator('#gateway-url');
    await url.fill(gateway.url);
    await page.locator('#gateway-token').fill('test-token');
    await page.locator('[data-action="gateway-connect"]').click();

    await expect(page.locator('#gateway-settings-status')).toContainText('Connected to 127.0.0.1');
    await expect(badge(page)).toContainText('Connected to 127.0.0.1');
    await expect(page.locator('#browser-toggle')).toBeHidden();

    await page.locator('[data-action="gateway-disconnect"]').click();
    await expect(badge(page)).toBeHidden();
    await expect(page.locator('#gateway-settings-status')).toContainText('Not connected');
    // The local transport answers again, so its surfaces come back.
    await expect(page.locator('#browser-toggle')).toBeVisible();
  } finally {
    await gateway.close();
  }
});

test('an unreachable gateway is reported, not silently swapped for the local database', async ({ page }) => {
  await page.goto('/');
  // Port 1 on loopback refuses connections immediately.
  await page.evaluate(() => (window as any).__connectGatewayForTest('http://127.0.0.1:1', 'test-token').catch(() => {}));
  await expect(badge(page)).toBeVisible();
  await expect(badge(page)).toContainText('Cannot reach');
});
