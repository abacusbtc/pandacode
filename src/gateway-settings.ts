// The app's side of a remote gateway: which backend this window talks to, the
// Settings row that changes it, and the surfaces a gateway cannot serve.
//
// The address is non-secret and the token lives in the OS keychain, both through the
// desktop shell's own commands (`gateway_config`, `gateway_connect`,
// `gateway_disconnect` in src-tauri/src/lib.rs) — never through the active command
// transport, because in remote mode that transport is the gateway itself.

import { createGatewayClient, type GatewayClient } from './gateway-client';
import { callShell, currentMode, preview, setMode, type Mode } from './bridge';
import { escape } from './dom';

/** The gateway's answer to `gateway_status` (D2/D4). */
type StatusReply = { commands?: string[]; features?: string[] };

type Connection = { url: string; commands: string[]; reachable: boolean };

let client: GatewayClient | null = null;
let connection: Connection | null = null;

/** The gateway this window uses, or `null` when it uses its own backend. */
export function gatewayStatus(): { url: string; mode: Mode } | null {
  return connection ? { url: connection.url, mode: currentMode() } : null;
}

/** Whether the connected gateway serves a command. The local transports serve everything. */
export function capabilityEnabled(command: string): boolean {
  return !connection || connection.commands.includes(command);
}

export function gatewayHost(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** The surfaces a gateway is not expected to serve, and the command that would make each real. */
const GATED: { command: string; selector: string }[] = [
  { command: 'browser_open', selector: '#browser-toggle, [data-action="browser"]' },
  { command: 'vm_status', selector: '[data-action="tab-vm"]' },
  { command: 'simulator_status', selector: '[data-action="tab-simulator"]' },
  { command: 'overlay_toggle', selector: '#launcher-heading, #launcher-settings' },
];

/**
 * Hides what the gateway cannot serve, and shows it again when it can. The attribute
 * is `data-gateway-hidden` rather than `hidden` on purpose: removing it restores the
 * platform's own decision (the VM tab is already hidden away from macOS) instead of
 * forcing a surface open. `null` means the local transports, so nothing is hidden.
 */
export function applyCapabilities(commands: string[] | null): void {
  for (const gated of GATED) {
    const served = !commands || commands.includes(gated.command);
    for (const element of document.querySelectorAll<HTMLElement>(gated.selector)) element.toggleAttribute('data-gateway-hidden', !served);
  }
  document.body.classList.toggle('gateway-remote', commands !== null);
}

/** The header badge: which backend this window talks to, and whether it answers. */
export function paintGatewayBadge(): void {
  const badge = document.querySelector<HTMLButtonElement>('#gateway-badge');
  if (!badge) return;
  badge.hidden = !connection;
  if (!connection) return;
  const where = gatewayHost(connection.url);
  const text = badge.querySelector<HTMLElement>('#gateway-badge-text');
  if (text) text.textContent = connection.reachable ? `Connected to ${where}` : `Cannot reach ${where}`;
  badge.dataset.state = connection.reachable ? 'connected' : 'unreachable';
  badge.title = connection.reachable
    ? `PandaCode is using the backend at ${connection.url}. Open Settings to disconnect.`
    : `No answer from ${connection.url}. Open Settings to fix the address, or disconnect to use this computer.`;
}

/**
 * Points every command and event at `url` and reports whether it answered. Remote mode
 * is entered first: a gateway that does not answer must not leave the app quietly
 * writing to the local database, so the badge says what happened instead.
 */
export async function attachGateway(url: string, token: string): Promise<boolean> {
  const base = url.trim().replace(/\/+$/, '');
  const next = createGatewayClient({ url: base, token });
  client?.close();
  client = next;
  connection = { url: base, commands: [], reachable: false };
  await setMode('remote', next);
  paintGatewayBadge();
  applyCapabilities([]);
  try {
    const status = await next.call<StatusReply>('gateway_status', {});
    const commands = Array.isArray(status?.commands) ? status.commands.filter((command): command is string => typeof command === 'string') : null;
    if (!commands) throw 'The gateway did not answer with a command list.';
    connection = { url: base, commands, reachable: true };
    applyCapabilities(commands);
  } catch {
    // Keep the client and the address: the next attempt or the next launch can retry,
    // and the person can still fix the address in Settings.
    paintGatewayBadge();
    return false;
  }
  paintGatewayBadge();
  return true;
}

/**
 * Boot's mode choice. Returns false when a saved gateway did not answer, so the caller
 * can say so rather than fall back silently.
 */
export async function startGatewaySession(): Promise<boolean> {
  let saved: { url: string; token: string } | null = null;
  try {
    saved = await callShell<{ url: string; token: string } | null>('gateway_config');
  } catch (error) {
    // An unreadable keychain is worth saying out loud, but it is not a reason to lose
    // the workspace: the app keeps its own backend.
    console.warn('Could not read the saved gateway', error);
  }
  if (!saved?.url || !saved.token) return true;
  return attachGateway(saved.url, saved.token);
}

/** The Settings row's Connect. Only a gateway that answered is remembered for next launch. */
export async function connectGateway(url: string, token: string): Promise<boolean> {
  const answered = await attachGateway(url, token);
  if (!answered) return false;
  await callShell('gateway_connect', { url: url.trim().replace(/\/+$/, ''), token });
  return true;
}

/** Forgets the gateway and goes back to this computer's own backend. */
export async function disconnectGateway(): Promise<void> {
  await callShell('gateway_disconnect').catch(() => undefined);
  client?.close();
  client = null;
  connection = null;
  await setMode(preview ? 'preview' : 'native');
  paintGatewayBadge();
  applyCapabilities(null);
}

export type GatewaySettingsOptions = {
  notify(message: string): void;
  /** Re-reads projects and sessions over the transport that is now active. */
  reload(): Promise<void>;
};

/** Settings → General → Remote gateway: address, token, Test, Connect, Disconnect. */
export function mountGatewaySettings(host: HTMLElement, options: GatewaySettingsOptions): () => void {
  const current = gatewayStatus();
  host.innerHTML = `<div class="settings-group gateway-group">
    <div class="setting-row"><div><strong>Gateway address</strong><small>Run pandacode-gateway on the machine that holds your projects, then connect here.</small></div><input id="gateway-url" type="url" inputmode="url" autocomplete="off" spellcheck="false" placeholder="http://127.0.0.1:4175" aria-label="Gateway address" value="${current ? escape(current.url) : ''}"></div>
    <div class="setting-row"><div><strong>Token</strong><small>Kept in your OS keychain. The gateway writes it to its data folder on first run.</small></div><input id="gateway-token" type="password" autocomplete="off" spellcheck="false" placeholder="${current ? 'Saved' : 'From the gateway’s data folder'}" aria-label="Gateway token"></div>
    <div class="setting-row"><div><strong>Connection</strong><small id="gateway-settings-status" role="status">${current ? `Connected to ${escape(gatewayHost(current.url))}` : 'Not connected'}</small></div><div class="settings-row-actions">
      <button type="button" class="secondary-button" data-action="gateway-test">Test</button>
      <button type="button" class="secondary-button" data-action="gateway-connect">Connect</button>
      <button type="button" class="secondary-button" data-action="gateway-disconnect"${current ? '' : ' disabled'}>Disconnect</button>
    </div></div>
  </div>`;

  const field = (id: string) => host.querySelector<HTMLInputElement>(`#${id}`)!;
  const line = () => host.querySelector<HTMLElement>('#gateway-settings-status')!;
  const say = (text: string, failed = false): void => {
    const status = line();
    status.textContent = text;
    status.classList.toggle('gateway-error', failed);
  };
  const busy = (working: boolean): void => {
    const connected = Boolean(gatewayStatus());
    for (const button of host.querySelectorAll<HTMLButtonElement>('[data-action^="gateway-"]')) {
      button.disabled = working || (button.dataset.action === 'gateway-disconnect' && !connected);
    }
  };

  async function perform(action: 'test' | 'connect' | 'disconnect'): Promise<void> {
    if (action === 'disconnect') {
      busy(true);
      try {
        await disconnectGateway();
        say('Not connected');
        options.notify('Disconnected. PandaCode uses this computer again.');
        await options.reload();
      } catch (error) {
        say(String(error), true);
      } finally {
        busy(false);
      }
      return;
    }
    const url = field('gateway-url').value.trim();
    const token = field('gateway-token').value.trim();
    if (!url || !token) {
      say('Enter the gateway address and its token.', true);
      return;
    }
    busy(true);
    try {
      if (action === 'test') {
        // A throwaway client: testing an address must not move the app onto it.
        const probe = createGatewayClient({ url, token });
        try {
          const status = await probe.call<StatusReply>('gateway_status', {});
          const count = Array.isArray(status?.commands) ? status.commands.length : 0;
          say(`${gatewayHost(url)} answers. It serves ${count} ${count === 1 ? 'command' : 'commands'}.`);
        } finally {
          probe.close();
        }
        return;
      }
      if (!(await connectGateway(url, token))) {
        say(`No answer from ${gatewayHost(url)}. Check the address, and that the gateway is running.`, true);
        return;
      }
      field('gateway-token').value = '';
      say(`Connected to ${gatewayHost(url)}`);
      await options.reload();
    } catch (error) {
      say(String(error), true);
    } finally {
      busy(false);
    }
  }

  const click = (event: MouseEvent): void => {
    const button = event.target instanceof Element ? event.target.closest<HTMLElement>('[data-action^="gateway-"]') : null;
    if (!button || button.hasAttribute('disabled')) return;
    void perform(button.dataset.action!.replace('gateway-', '') as 'test' | 'connect' | 'disconnect');
  };
  host.addEventListener('click', click);
  return () => host.removeEventListener('click', click);
}
