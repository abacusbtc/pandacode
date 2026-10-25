import { expect, test, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

/* The shipping webview policy, applied to a page by effect (PAN-150).
 *
 * This is the gap that hid the defect: the browser preview never applies the Tauri CSP, so
 * "frontend remote turns" could be proven in a browser while a packaged app refused every
 * gateway request before it left the machine. Here the policy from `tauri.conf.json` is the
 * policy under test — served as the page's own `Content-Security-Policy` header, the way the
 * packaging applies it — and each gateway is answered by a route interceptor, so no network
 * leaves this host. What is measured is only whether the policy lets the request out.
 *
 * `tests/tauri-config.test.mjs` is the other half: it reads the directives, which is how
 * `wss:` (an upgrade a route cannot stand in for) is covered.
 *
 * `PANDACODE_TAURI_CONF` points this file at a copy of another configuration; that is how
 * the negative proof runs it against the previous policy. */
const csp = (): string => JSON.parse(readFileSync(process.env.PANDACODE_TAURI_CONF ?? 'src-tauri/tauri.conf.json', 'utf8')).app.security.csp;

/** A probe page on the app's own origin, carrying the policy exactly as the app ships it. */
async function underPolicy(page: Page): Promise<string[]> {
  const blocked: string[] = [];
  page.on('console', message => { if (/Content Security Policy/i.test(message.text())) blocked.push(message.text()); });
  await page.route('**/csp-probe.html', route => route.fulfill({
    contentType: 'text/html',
    headers: { 'Content-Security-Policy': csp() },
    body: '<!doctype html><title>policy probe</title><body>policy probe</body>',
  }));
  await page.goto('/csp-probe.html');
  await expect(page).toHaveTitle('policy probe');
  return blocked;
}

/** One call through the policy, answered locally: `allowed` when the request got out. */
async function call(page: Page, url: string): Promise<{ allowed: boolean; status?: number; body?: string }> {
  return page.evaluate(async url => {
    try {
      const response = await fetch(url, { method: 'POST', body: '{}' });
      return { allowed: true, status: response.status, body: await response.text() };
    } catch {
      return { allowed: false };
    }
  }, url);
}

test('a packaged app reaches a gateway over TLS', async ({ page }) => {
  const blocked = await underPolicy(page);
  await page.route('https://gateway.example.ts.net/rpc', route => route.fulfill({
    status: 200,
    body: '{"ok":true}',
    headers: { 'access-control-allow-origin': '*', 'content-type': 'application/json' },
  }));
  expect(await call(page, 'https://gateway.example.ts.net/rpc')).toEqual({ allowed: true, status: 200, body: '{"ok":true}' });
  expect(blocked.filter(message => message.includes('gateway.example.ts.net'))).toEqual([]);
});

test('a gateway on this computer answers over loopback, as Settings suggests', async ({ page }) => {
  // Settings → Gateway address offers `http://127.0.0.1:4175` as the example.
  await underPolicy(page);
  await page.route('http://127.0.0.1:4175/healthz', route => route.fulfill({ status: 200, body: 'ok', headers: { 'access-control-allow-origin': '*' } }));
  expect(await call(page, 'http://127.0.0.1:4175/healthz')).toEqual({ allowed: true, status: 200, body: 'ok' });
});

test('a cleartext gateway on the LAN is still refused, and the refusal names connect-src', async ({ page }) => {
  const blocked = await underPolicy(page);
  // Reachable if the policy let it through: the only thing that can refuse it is connect-src.
  await page.route('http://192.168.1.10:4175/rpc', route => route.fulfill({ status: 200, body: '{"ok":true}', headers: { 'access-control-allow-origin': '*' } }));
  expect(await call(page, 'http://192.168.1.10:4175/rpc')).toEqual({ allowed: false });
  await expect.poll(() => blocked.join('\n')).toContain('http://192.168.1.10:4175');
  expect(blocked.join('\n')).toMatch(/connect-src/);
});
