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

test('a project added through the real gateway is in the browser, and the fixtures are not', async ({ page }) => {
  // A cold `cargo run --features gateway` on a loaded host is minutes, not seconds.
  test.setTimeout(300_000);
  expect(
    existsSync(join(STATIC_DIR, 'index.html')),
    'the gateway serves the built app: run `npm run build` first (npm run gateway:test does it)',
  ).toBe(true);

  const data = mkdtempSync(join(tmpdir(), 'pc-gw-data-'));
  // The project name is the folder's own name, as `Store::add_project` derives it.
  const repo = realpathSync(mkdtempSync(join(tmpdir(), 'pc-gw-repo-')));
  const name = basename(repo);
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
    const token = readFileSync(join(data, 'gateway-token'), 'utf8').trim();
    expect(token, 'the gateway wrote its own token on first run').toMatch(/^[0-9a-f]{64}$/);

    await openApp(page, base);
    await page.evaluate(
      ([url, secret]) => (window as any).__connectGatewayForTest(url, secret),
      [base, token],
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
    const projects = await rpc(base, token, 'list_projects') as { name: string; path: string }[];
    expect(projects.map(project => project.path)).toEqual([repo]);

    // The fixture's text is nowhere in the document, not merely out of the sidebar.
    await expect(page.locator('body')).not.toContainText(FIXTURE_PROJECT);
  } finally {
    await stop(child);
    // Cleanup never becomes the failure: a directory that cannot be removed is a
    // leftover in the system temp directory, not a reason to hide the real error.
    for (const directory of [data, repo]) {
      try {
        rmSync(directory, { recursive: true, force: true });
      } catch {
        // Left for the OS to reap.
      }
    }
  }
});
