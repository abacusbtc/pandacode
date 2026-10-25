import { expect, test, type Page } from '@playwright/test';
import { gatewayRunnable, recordGateway } from './gateway-recording';

// PAN-101: relaunching the app with a saved gateway must talk to that gateway only
// about what it serves, and a Settings pane must gate on capability instead of showing
// a person `unsupported_command`.
//
// The server here is the **real** `pandacode-gateway` (its own audit is the recording),
// the app is the built one served by that same gateway, and the only app-side hook used
// is `__connectGatewayForTest` — the same connect path the Settings row takes.

const openApp = async (page: Page, base: string): Promise<void> => {
  await page.goto(`${base}/`);
  await page.waitForFunction(() => typeof (window as any).__connectGatewayForTest === 'function');
};

const connected = async (page: Page): Promise<void> => {
  await expect(page.locator('#gateway-badge')).toContainText('Connected to', { timeout: 20_000 });
};

/** Saves the gateway the way Settings → Connect does, so the next launch reads it. */
async function saveGateway(page: Page, base: string, token: string): Promise<void> {
  const answered = await page.evaluate(
    ([url, token]) => (window as any).__connectGatewayForTest(url, token),
    [base, token],
  );
  expect(answered, 'the gateway refused the connect').toBe(true);
  await connected(page);
}

/**
 * Boots with the saved gateway and returns every command the gateway journalled.
 *
 * "First idle" is measured: the list is read once it has stopped growing for 1.5 s,
 * after at least 2 s of boot, so a late fetch is not missed and a quiet boot does not
 * hide behind a fixed wait.
 */
async function bootAndRecord(page: Page, gateway: { commands(): string[] }, base: string): Promise<string[]> {
  await openApp(page, base);
  await connected(page);
  let previous = -1;
  let stableSince = Date.now();
  const started = Date.now();
  while (Date.now() - started < 25_000) {
    const count = gateway.commands().length;
    if (count !== previous) {
      previous = count;
      stableSince = Date.now();
    } else if (Date.now() - stableSince > 1_500 && Date.now() - started > 2_000) {
      break;
    }
    await page.waitForTimeout(100);
  }
  return gateway.commands();
}

/** Settings sections, in the order the tab strip renders them. */
const SECTIONS = ['general', 'appearance', 'agents', 'providers', 'teams', 'tools', 'cloud', 'github', 'about'];

/**
 * Opens Settings and every pane in it, returning the commands the gateway journalled
 * while it did. A pane is driven through its own tab, so the path is the person's.
 */
async function visitSettings(page: Page, gateway: { commands(): string[] }): Promise<string[]> {
  gateway.clear();
  await page.locator('[data-action="settings"]').first().click();
  await expect(page.locator('#settings-tab-general')).toBeVisible({ timeout: 15_000 });
  for (const section of SECTIONS) {
    await page.locator(`#settings-tab-${section}`).click();
    // The pane mounts lazily; a settled panel is the signal that its fetches started.
    await page.waitForTimeout(400);
  }
  return gateway.commands();
}

test('boot and every Settings pane against a saved gateway send only served commands', async ({ page }) => {
  test.skip(!gatewayRunnable(), 'build the app first: npm run build (npm run gateway:test does both)');
  const gateway = await recordGateway();
  try {
    await openApp(page, gateway.base);
    await saveGateway(page, gateway.base, gateway.token);

    // The relaunch: boot must read the saved gateway and route its own fetches there.
    gateway.clear();
    const booted = await bootAndRecord(page, gateway, gateway.base);
    const { commands: served } = await gateway.advertised();
    // Settings is the other half of the ticket: every pane, opened the way a person
    // opens it, on the same real gateway. Both captures happen before any assertion so
    // one defect does not hide the other.
    const panes = await visitSettings(page, gateway);
    console.log(`BOOT COMMANDS (${booted.length}): ${JSON.stringify(booted)}`);
    console.log(`SETTINGS COMMANDS (${panes.length}): ${JSON.stringify(panes)}`);

    const ungated = [...new Set([...booted, ...panes])].filter(command => !served.includes(command));
    expect(ungated, `ungated against this gateway: ${ungated.join(', ')}`).toEqual([]);

    // Proof 3: the panes that cannot work say so in the person's terms. Each sentence is
    // the pane's own, asserted where a person would read it.
    const panel = (section: string) => page.locator(`#settings-panel-${section}`);
    await page.locator('#settings-tab-providers').click();
    await expect(panel('providers').locator('.settings-embed-unserved')).toContainText('does not serve provider settings');
    await page.locator('#settings-tab-teams').click();
    await expect(panel('teams')).toContainText('does not serve named agent teams');
    await page.locator('#settings-tab-cloud').click();
    await expect(panel('cloud')).toContainText('does not serve cloud sandboxes');
    // The theme library is the client's own, so it is NOT gated: the pane still lists
    // themes, and its command never crosses /rpc (asserted on the recording below).
    await page.locator('#settings-tab-appearance').click();
    await expect(panel('appearance').locator('.vsix-choice').first()).toBeVisible();
    expect(booted.concat(panes)).not.toContain('themes_list');
    await page.locator('#settings-tab-about').click();
    await expect(panel('about').locator('.settings-resources')).toContainText('does not report resource usage');

    // Nothing reached the person as a raw transport string, anywhere.
    const toast = (await page.locator('#toast').textContent()) ?? '';
    expect(toast).not.toContain('unsupported_command');
    const body = (await page.locator('body').textContent()) ?? '';
    expect(body).not.toContain('unsupported_command');
  } finally {
    await gateway.stop();
  }
});
