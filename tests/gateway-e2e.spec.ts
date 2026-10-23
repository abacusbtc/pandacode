// The end-to-end proof behind docs/gateway.md: the real `pandacode-gateway` binary,
// a real (empty) database in a temporary data directory, the real built app served by
// that binary, and a real browser. A project added through the UI must reach the
// gateway's database and come back into the sidebar; not one of the browser preview's
// fixture projects may.
//
// Nothing here is a fixture and nothing is mocked: the only app-side hook used is
// `__connectGatewayForTest`, which is the same connect path the Settings row takes
// (src/main.ts). The project is added through the app's own "Open project" form, so
// `add_project` runs over /rpc from the browser, exactly as a person's click would.

import { expect, test, type Page } from '@playwright/test';
import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync } from 'node:fs';
import { createServer } from 'node:net';
import { homedir, tmpdir } from 'node:os';
import { basename, join } from 'node:path';

const ROOT = process.cwd();
/** The Rust package is its own workspace; `cargo run` has to be pointed at it. */
const MANIFEST = join(ROOT, 'src-tauri', 'Cargo.toml');
/** The gateway serves the built app; `npm run build` writes it. */
const STATIC_DIR = join(ROOT, 'dist');
/** The browser preview's sample projects (src/preview.ts), and what must never show. */
const FIXTURE_PROJECT = 'orbit';

/**
 * Cargo is not on every PATH: this host keeps it in ~/.cargo/bin, which a test spawned
 * from `npm run` does not necessarily inherit. `CARGO` overrides both.
 */
function cargoCommand(): string {
  if (process.env.CARGO) return process.env.CARGO;
  const local = join(homedir(), '.cargo', 'bin', 'cargo');
  return existsSync(local) ? local : 'cargo';
}

/** A port the kernel just handed out, so a parallel run cannot take the gateway's. */
function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once('error', reject);
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address();
      const port = typeof address === 'object' && address ? address.port : 0;
      probe.close(() => (port ? resolve(port) : reject(new Error('no port was assigned'))));
    });
  });
}

/**
 * Waits for the gateway to answer /healthz, which is unauthenticated by design — the
 * client needs it before it has read the token. There is no fixed sleep: a compile and a
 * cold database take as long as they take, and a gateway that dies is reported with the
 * child's own output rather than as a timeout.
 */
async function waitForHealth(base: string, child: ChildProcess, output: () => string): Promise<void> {
  const deadline = Date.now() + 240_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`the gateway exited with ${child.exitCode} before answering /healthz:\n${output()}`);
    }
    try {
      const response = await fetch(`${base}/healthz`);
      if (response.ok && (await response.text()).trim() === 'ok') return;
    } catch {
      // Not listening yet, or mid-shutdown; the next attempt is the answer.
    }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`the gateway did not answer /healthz within 240 s:\n${output()}`);
}

/** One JSON-RPC call as the client makes it: the gateway's own envelope, over HTTP. */
async function rpc(base: string, token: string, command: string, args: Record<string, unknown> = {}): Promise<any> {
  const response = await fetch(`${base}/rpc`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ id: 'e2e', command, args }),
  });
  const body = await response.json() as { ok?: boolean; result?: unknown; error?: string };
  if (!response.ok || !body.ok) throw new Error(`${command} failed: ${body.error ?? response.status}`);
  return body.result;
}

/** The app's own entry point, then the fixture workspace boot rendered from sample data. */
async function openApp(page: Page, base: string): Promise<void> {
  await page.goto(`${base}/`);
  await page.waitForFunction(() => typeof (window as any).__connectGatewayForTest === 'function');
  // The app is briefly the browser preview: this is the control for the assertions
  // below, so "the fixture is gone" cannot pass because nothing was ever there.
  await expect(page.getByRole('region', { name: `${FIXTURE_PROJECT} tasks`, exact: true })).toBeVisible();
}

async function stop(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise<void>(resolve => child.once('exit', () => resolve()));
  child.kill('SIGTERM');
  const forced = setTimeout(() => child.kill('SIGKILL'), 10_000);
  await exited;
  clearTimeout(forced);
}

type Gateway = { base: string; token: string; stop(): Promise<void> };

/** The real gateway on a free port, serving the built app, over its own fresh database. */
async function startGateway(): Promise<Gateway> {
  expect(
    existsSync(join(STATIC_DIR, 'index.html')),
    'the gateway serves the built app: run `npm run build` first (npm run gateway:test does it)',
  ).toBe(true);

  const data = mkdtempSync(join(tmpdir(), 'pc-gw-data-'));
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  let output = '';
  // No --token-file: an explicit path that does not exist yet is a refusal, and first run
  // is exactly that case. Without it the gateway writes <data-dir>/gateway-token, 0600.
  const child = spawn(cargoCommand(), [
    'run', '--quiet', '--manifest-path', MANIFEST, '--features', 'gateway', '--bin', 'pandacode-gateway', '--',
    '--data-dir', data, '--static-dir', STATIC_DIR, '--host', '127.0.0.1', '--port', String(port),
  ], { cwd: ROOT, shell: false });
  child.stdout?.on('data', chunk => { output += chunk; });
  child.stderr?.on('data', chunk => { output += chunk; });

  try {
    await waitForHealth(base, child, () => output);
  } catch (error) {
    await stop(child);
    throw error;
  }
  const token = readFileSync(join(data, 'gateway-token'), 'utf8').trim();
  return {
    base,
    token,
    async stop() {
      await stop(child);
      // Cleanup never becomes the failure: a directory that cannot be removed is a
      // leftover in the system temp directory, not a reason to hide the real error.
      try { rmSync(data, { recursive: true, force: true }); } catch { /* Left for the OS to reap. */ }
    },
  };
}

test('a project added through the real gateway is in the browser, and the fixtures are not', async ({ page }) => {
  // A cold `cargo run --features gateway` on a loaded host is minutes, not seconds.
  test.setTimeout(300_000);
  const gateway = await startGateway();
  expect(gateway.token, 'the gateway wrote its own token on first run').toMatch(/^[0-9a-f]{64}$/);
  // The project name is the folder's own name, as `Store::add_project` derives it.
  const repo = realpathSync(mkdtempSync(join(tmpdir(), 'pc-gw-repo-')));
  const name = basename(repo);

  try {
    await openApp(page, gateway.base);
    await page.evaluate(
      ([url, secret]) => (window as any).__connectGatewayForTest(url, secret),
      [gateway.base, gateway.token],
    );
    await expect(page.locator('#gateway-badge')).toContainText('Connected to 127.0.0.1');

    // The workspace is now the gateway's: an empty database, so the sample projects go.
    await expect(page.getByRole('region', { name: `${FIXTURE_PROJECT} tasks`, exact: true })).toHaveCount(0);
    await expect(page.locator('#navigation')).not.toContainText(FIXTURE_PROJECT);

    // A person's path through the app: sidebar → Open project → the folder's own form.
    await page.locator('.sidebar-projects [data-action="add-project"]').click();
    await page.locator('#project-folder').fill(repo);
    await page.locator('#open-project-form .primary-button').click();
    await expect(page.getByRole('region', { name: `${name} tasks`, exact: true })).toBeVisible();

    // ...and it is in the gateway's database, not in the page's memory: ask the same
    // endpoint independently of everything the app just did.
    const projects = await rpc(gateway.base, gateway.token, 'list_projects') as { name: string; path: string }[];
    expect(projects.map(project => project.path)).toEqual([repo]);

    // The fixture's text is nowhere in the document, not merely out of the sidebar.
    await expect(page.locator('body')).not.toContainText(FIXTURE_PROJECT);
  } finally {
    await gateway.stop();
    try { rmSync(repo, { recursive: true, force: true }); } catch { /* Left for the OS to reap. */ }
  }
});

/**
 * PAN-86/PAN-91/PAN-100: a connected workspace must ask the gateway only for commands
 * it advertises, must not call itself a preview, and must not offer the .vsix import
 * that has no upload route. The record is the gateway's own answer to every /rpc the
 * page made, so a surface that quietly re-adds a call turns this red.
 */
test('a workspace loaded over the real gateway asks it only for what it serves', async ({ page }) => {
  test.setTimeout(300_000);
  const gateway = await startGateway();
  // A workspace with something in it: an empty database loads no project and asks no
  // surface for anything, which would make this proof pass for the wrong reason.
  // Both rows are written through the gateway's own commands, not the app's UI.
  const repo = realpathSync(mkdtempSync(join(tmpdir(), 'pc-gw-repo-')));
  const project = await rpc(gateway.base, gateway.token, 'add_project', { path: repo }) as { id: string };
  await rpc(gateway.base, gateway.token, 'create_session', { projectId: project.id, provider: 'claude', model: 'sonnet' });
  /** Every /rpc answer the page received, with the command it answered. */
  const answers: { command: string; error: string }[] = [];
  const recording: Promise<void>[] = [];
  page.on('response', response => {
    if (!response.url().endsWith('/rpc')) return;
    recording.push((async () => {
      let command = '?';
      try { command = String((response.request().postDataJSON() as { command?: string })?.command ?? '?'); }
      catch { /* a body that is not JSON is not one of the app's own commands */ }
      let error = '';
      try { error = String((await response.json() as { error?: unknown }).error ?? ''); }
      catch { /* an HTTP-level refusal, not a command answer */ }
      answers.push({ command, error });
    })());
  });
  /** The Colors pane's import row lives in Settings → Appearance. */
  async function openColors(): Promise<void> {
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('region', { name: 'Settings', exact: true }).getByRole('tab', { name: 'Appearance', exact: true }).click();
    await expect(page.locator('.vsix-list')).toHaveAttribute('aria-busy', 'false');
  }
  const importVsix = page.locator('[data-colors="import"]');

  try {
    await openApp(page, gateway.base);
    // The control: this window really is the browser preview before it connects, so the
    // sample-data label and the import row are both there to be lost.
    await expect(page.locator('#preview-status')).toContainText('sample data');
    await openColors();
    await expect(importVsix).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('#settings-page')).toBeHidden();

    await page.evaluate(
      ([url, secret]) => (window as any).__connectGatewayForTest(url, secret),
      [gateway.base, gateway.token],
    );
    await expect(page.locator('#gateway-badge')).toContainText('Connected to 127.0.0.1');
    await expect(page.getByRole('region', { name: `${FIXTURE_PROJECT} tasks`, exact: true })).toHaveCount(0);
    await page.waitForLoadState('networkidle');
    await Promise.all(recording);

    const unsupported = answers.filter(answer => answer.error.includes('unsupported_command'));
    expect(answers.length, 'the page really did talk to the gateway: an empty record would pass vacuously').toBeGreaterThan(0);
    expect(
      [...new Set(unsupported.map(answer => answer.command))],
      `the workspace asked for commands the gateway does not serve (${answers.length} answers recorded)`,
    ).toEqual([]);

    // It still says which backend answers, and no longer claims to be sample data.
    await expect(page.locator('#preview-status')).toBeHidden();
    // `innerText` rather than `textContent`: a hidden claim is not on the screen.
    expect(await page.locator('body').innerText()).not.toContain('sample data');

    // The import row is gone while the gateway cannot accept a file, and the gateway's
    // own capability list is what decides that — not a hardcoded remote check.
    await openColors();
    await expect(importVsix).toBeHidden();
  } finally {
    await gateway.stop();
    try { rmSync(repo, { recursive: true, force: true }); } catch { /* Left for the OS to reap. */ }
  }
});

