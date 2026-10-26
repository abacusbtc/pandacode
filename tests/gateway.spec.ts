import { expect, test, type Page } from '@playwright/test';
import { startGateway, type Replies } from './gateway-stub';

// The connect UI against a stub gateway (tests/gateway-stub.ts) that answers the same
// JSON-RPC envelope and numbered event stream the real server does.

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

/**
 * boot() attaches its event subscriptions before it renders the workspace, so a listed
 * sample project means the preview transport already owns them. A test that connects
 * before this point would race boot into subscribing straight to the gateway, which
 * would hide a missing rebind on the runtime switch this suite is here to check.
 */
async function waitForBoot(page: Page): Promise<void> {
  await expect(page.getByRole('region', { name: 'orbit tasks', exact: true })).toBeVisible();
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

test('a runtime connect moves event subscriptions onto the gateway', async ({ page }) => {
  // The preview boots on its own transport; connecting from Settings has to move the
  // already-attached subscriptions, not leave them listening to this computer.
  const gateway = await startGateway(seeded());
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await expect.poll(() => gateway.sockets(), { message: 'the app opened the gateway event socket' }).toBeGreaterThan(0);

    // A frame from the gateway now reaches the handlers boot registered.
    const before = gateway.calls.filter(command => command === 'list_sessions').length;
    gateway.emit('sessions-changed');
    await expect.poll(() => gateway.calls.filter(command => command === 'list_sessions').length).toBeGreaterThan(before);
  } finally {
    await gateway.close();
  }
});
test('a remote session reload never asks the gateway for get_task', async ({ page }) => {
  // The list row already carries what get_task would, so a reload does not spend a round
  // trip per event on it.
  const gateway = await startGateway(seeded());
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await expect.poll(() => gateway.sockets()).toBeGreaterThan(0);

    const before = gateway.calls.filter(command => command === 'list_sessions').length;
    gateway.emit('sessions-changed');
    await expect.poll(() => gateway.calls.filter(command => command === 'list_sessions').length).toBeGreaterThan(before);

    expect(gateway.calls).not.toContain('get_task');
    // The reload finished: the task the gateway listed is still the open one.
    await expect(page.locator('#navigation')).toContainText('Seeded from the server');
  } finally {
    await gateway.close();
  }
});

test('the Settings Test probe reports the gateway without moving onto it', async ({ page }) => {
  // "Test" builds a throwaway client: it must answer the question without leaving the
  // window on an address nobody chose to connect to.
  const gateway = await startGateway(seeded());
  try {
    await openApp(page);
    await waitForBoot(page);
    await page.locator('.sidebar-bottom [data-action="settings"]').click();
    await page.locator('#gateway-url').fill(gateway.url);
    await page.locator('#gateway-token').fill('test-token');
    await page.locator('[data-action="gateway-test"]').click();
    await expect(page.locator('#gateway-settings-status')).toContainText('answers. It serves 7 commands.');

    // A probe asks only what the gateway serves; a connection would reload the workspace.
    expect(gateway.calls).toContain('gateway_status');
    expect(gateway.calls).not.toContain('list_projects');
    await page.locator('[data-settings-close]').click();
    await expect(badge(page)).toBeHidden();
    await expect(page.getByRole('region', { name: 'orbit tasks', exact: true })).toBeVisible();
  } finally {
    await gateway.close();
  }
});

// ---- Task 10: the remote folder picker -------------------------------------------------
// The native dialog returns a path on *this* computer, which does not exist on the
// gateway (D6). These drive the real surfaces: the sidebar's Open project button and
// the form's Browse…

test('picks a project folder from the server in remote mode', async ({ page }) => {
  let projects = [PROJECT];
  const gateway = await startGateway({
    ...seeded([...M1, 'gateway_list_dirs']),
    list_projects: () => projects,
    // Task 7's shape: `path`/`parent` plus folder entries; only folders are listed.
    // The root answer is held for a moment so the loading state is observable.
    gateway_list_dirs: (args: any) => args?.path
      ? { path: args.path, parent: '/srv/repos', entries: [] }
      : new Promise(resolve => setTimeout(() => resolve({ path: '/srv/repos', parent: '/srv', entries: [{ name: 'pandacode', isDir: true }, { name: 'notes.txt', isDir: false }] }), 300)),
    add_project: (args: any) => {
      const path = String(args?.path);
      const added = { id: 'project-srv-2', name: path.split('/').pop(), path };
      projects = [...projects, added];
      return added;
    },
  });
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);

    await page.locator('.sidebar-projects [data-action="add-project"]').click();
    await page.locator('[data-action="browse-project"]').click();

    // While the server has not answered, no folder can be chosen.
    await expect(page.locator('.remote-files .use-folder')).toBeDisabled();
    await expect(page.locator('.remote-files-list')).toContainText('Loading…');

    // The server's listing, folders only — notes.txt is not offered.
    await expect(page.locator('.remote-files .entry')).toHaveText(['pandacode']);
    await page.locator('.remote-files .entry').click();
    await expect(page.locator('.remote-files-path')).toHaveText('/srv/repos/pandacode');
    await expect(page.locator('.remote-files-list')).toContainText('No folders here');

    // "Use this folder" returns the server path to the form, and opening it creates the
    // project on the gateway.
    await page.locator('.remote-files .use-folder').click();
    await expect(page.locator('#project-folder')).toHaveValue('/srv/repos/pandacode');
    await page.locator('#open-project-form .primary-button').click();
    // The new project is its own region in the sidebar; the region's name is an
    // aria-label, so the text of #navigation does not carry it.
    await expect(page.getByRole('region', { name: 'pandacode tasks', exact: true })).toBeVisible();
    expect(gateway.calls).toContain('add_project');
  } finally {
    await gateway.close();
  }
});

test('the server picker reports a server error, walks up, and closes on Escape', async ({ page }) => {
  const gateway = await startGateway({
    ...seeded([...M1, 'gateway_list_dirs']),
    gateway_list_dirs: (args: any) => {
      const path = args?.path ?? null;
      // The root is the server's starting folder, and asking for it by name must answer
      // the same way — that is what Backspace does.
      if (path === null || path === '/srv/repos') return { path: '/srv/repos', parent: null, entries: [{ name: 'private', isDir: true }] };
      throw new Error(`not_a_directory: ${path}`);
    },
  });
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await page.locator('.sidebar-projects [data-action="add-project"]').click();
    await page.locator('[data-action="browse-project"]').click();
    await expect(page.locator('.remote-files .entry')).toHaveText(['private']);

    // The server's own string, not a stack trace, and no folder is offered to use.
    await page.locator('.remote-files .entry').click();
    await expect(page.locator('.remote-files')).toContainText('not_a_directory: /srv/repos/private');
    await expect(page.locator('.remote-files .use-folder')).toBeDisabled();

    // Backspace walks back up to the listing that works.
    await page.keyboard.press('Backspace');
    await expect(page.locator('.remote-files .entry')).toHaveText(['private']);

    // Escape closes the picker: the dialog is hidden and no picker is reachable.
    await page.keyboard.press('Escape');
    await expect(page.locator('.remote-files')).toBeHidden();
    await expect(page.locator('#dialog')).not.toHaveAttribute('open', '');
  } finally {
    await gateway.close();
  }
});

test('dismissing the picker from its backdrop settles it and leaves no key handler behind', async ({ page }) => {
  // main.ts closes the dialog on a backdrop press (outsideDialog -> closeModal), a path
  // that never runs the picker's own Cancel. If that left the keydown handler attached,
  // Backspace in the app would keep asking the server for folders.
  const gateway = await startGateway({
    ...seeded([...M1, 'gateway_list_dirs']),
    gateway_list_dirs: () => ({ path: '/srv/repos', parent: '/srv', entries: [{ name: 'pandacode', isDir: true }] }),
  });
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await page.locator('.sidebar-projects [data-action="add-project"]').click();
    await page.locator('[data-action="browse-project"]').click();
    await expect(page.locator('.remote-files .entry')).toHaveText(['pandacode']);

    // The backdrop is the dialog's own area outside its panel.
    await page.mouse.click(4, 4);
    await expect(page.locator('#dialog')).not.toHaveAttribute('open', '');

    const before = gateway.calls.filter(command => command === 'gateway_list_dirs').length;
    await page.keyboard.press('Backspace');
    // Give a leaked handler time to fire before concluding there is none.
    await expect.poll(() => gateway.calls.filter(command => command === 'gateway_list_dirs').length, { timeout: 1500 }).toBe(before);
  } finally {
    await gateway.close();
  }
});

test('arrow keys walk the folder list and Enter opens the highlighted folder', async ({ page }) => {
  const asked: string[] = [];
  const gateway = await startGateway({
    ...seeded([...M1, 'gateway_list_dirs']),
    gateway_list_dirs: (args: any) => {
      const path = args?.path ?? null;
      asked.push(path ?? '<home>');
      if (path === null || path === '/srv/repos') {
        return { path: '/srv/repos', parent: '/srv', entries: [
          { name: 'alpha', isDir: true }, { name: 'beta', isDir: true }, { name: 'gamma', isDir: true },
        ] };
      }
      return { path, parent: '/srv/repos', entries: [] };
    },
  });
  try {
    await openApp(page);
    await waitForBoot(page);
    await connect(page, gateway);
    await page.locator('.sidebar-projects [data-action="add-project"]').click();
    await page.locator('[data-action="browse-project"]').click();
    await expect(page.locator('.remote-files .entry')).toHaveText(['alpha', 'beta', 'gamma']);

    // Opening the picker focuses the first folder, so the keys work without a click.
    await expect(page.locator('.remote-files .entry').first()).toBeFocused();
    await page.keyboard.press('ArrowDown');
    await expect(page.locator('.remote-files .entry').nth(1)).toBeFocused();
    await page.keyboard.press('ArrowDown');
    await expect(page.locator('.remote-files .entry').nth(2)).toBeFocused();
    await page.keyboard.press('ArrowUp');
    await expect(page.locator('.remote-files .entry').nth(1)).toBeFocused();

    // Enter opens the highlighted folder, not the first or the last one.
    await page.keyboard.press('Enter');
    await expect.poll(() => asked[asked.length - 1]).toBe('/srv/repos/beta');
    await expect(page.locator('.remote-files-path')).toHaveText('/srv/repos/beta');
  } finally {
    await gateway.close();
  }
});
