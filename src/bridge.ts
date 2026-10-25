import type { AgentEvent, BrowserElement, BrowserStatus, TerminalEvent } from './types';
import type { GatewayClient, MissedFrames } from './gateway-client';

export const preview = !('__TAURI_INTERNALS__' in window);

/** Where the app's commands go: Tauri IPC, the browser preview, or a remote gateway. */
export type Mode = 'native' | 'preview' | 'remote';
let mode: Mode = preview ? 'preview' : 'native';
let remote: GatewayClient | null = null;

/** One live subscription, kept so a transport change can move it instead of orphaning it. */
type Subscription = { name: string; callback: (event: any) => void };
const subscriptions = new Set<Subscription>();
const disposers = new Map<Subscription, () => void | Promise<void>>();

/** Attaches one subscription to whichever transport is active right now. */
async function attach(sub: Subscription): Promise<void> {
  let dispose: () => void | Promise<void>;
  if (mode === 'remote') {
    if (!remote) throw 'gateway_disconnected: no gateway is connected';
    dispose = await remote.subscribe(sub.name, sub.callback);
  } else if (mode === 'preview') dispose = (await import('./preview')).listen(sub.name, sub.callback);
  else dispose = await (await import('@tauri-apps/api/event')).listen(sub.name, event => sub.callback(event.payload));
  disposers.set(sub, dispose);
}

/**
 * Moves every live subscription to the transport that is now active. Without this a
 * window connected at runtime would keep reacting to the backend it left behind: the
 * desktop process still runs (and still emits) while the workspace shows gateway data.
 */
export async function rebindSubscriptions(): Promise<void> {
  if (!subscriptions.size) return;
  const entries = [...disposers];
  disposers.clear();
  for (const [, dispose] of entries) {
    try { await dispose(); } catch { /* a transport that already closed cannot unsubscribe */ }
  }
  for (const sub of subscriptions) {
    try { await attach(sub); } catch (error) { console.warn(`Could not move the ${sub.name} subscription`, error); }
  }
}

/** Everything that must restart when the active transport changes. */
const transportListeners = new Set<() => void>();

/** Remote mode is chosen at boot, before the first `call`. */
export async function setMode(next: Mode, client?: GatewayClient | null): Promise<void> {
  // A replaced or discarded remote client stops its socket and reconnect timer.
  if (remote && remote !== client) remote.close();
  const changed = mode !== next || remote !== (client ?? null);
  mode = next;
  remote = client ?? null;
  await rebindSubscriptions();
  if (!changed) return;
  // A terminal's PTY belongs to the backend that started it: its id means nothing on
  // the transport that is now active, so a console restarts rather than typing into a
  // shell that is no longer there. Everything that holds backend-owned state opts in
  // here instead of each caller watching the mode itself.
  for (const listener of transportListeners) {
    try { listener(); } catch (error) { console.warn('A transport-change handler failed', error); }
  }
}
/** Runs `listener` whenever the active transport changes. Returns an unsubscribe. */
export function onTransportChange(listener: () => void): () => void {
  transportListeners.add(listener);
  return () => transportListeners.delete(listener);
}
export function currentMode(): Mode {
  return mode;
}

/**
 * Commands only this computer answers, in every mode: the gateway settings themselves
 * and the native dialogs. In remote mode the gateway is the command transport, so it
 * cannot be asked to save the address it is reached at.
 */
export async function callShell<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (preview) return (await import('./preview')).invoke<T>(command, args);
  return (await import('@tauri-apps/api/core')).invoke<T>(command, args);
}
export async function call<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (mode === 'remote') {
    if (!remote) throw 'gateway_disconnected: no gateway is connected';
    return remote.call<T>(command, args);
  }
  return callShell<T>(command, args);
}
/**
 * Sends one file to the gateway that this window is driving.
 *
 * This is the only path an attachment takes in remote mode: the file's bytes go to
 * the gateway, which answers with an id a message can name. Nothing here reads the
 * file for the prompt — a document's text comes back through the agent's context,
 * and an image previews from the local file the person picked.
 */
export async function uploadAttachment(
  file: Blob,
  name: string,
  kind: 'image' | 'document',
  onProgress?: (sent: number, total: number) => void,
): Promise<import('./gateway-client').UploadReceipt> {
  if (mode !== 'remote' || !remote) throw new Error('Uploading needs a connected gateway.');
  return remote.upload(file, name, kind, onProgress);
}
/**
 * One terminal's frames, from a gateway.
 *
 * This is a socket filtered to that terminal (`?terminal=`), replayed from the start of
 * its ring: that is both the scrollback a console opening late needs and how the gateway
 * knows somebody is watching it. The app's own transports deliver terminal frames on
 * their shared stream instead — terminal.ts subscribes before the PTY exists, because
 * there is no ring there to replay what it missed.
 */
export async function watchTerminal(
  terminalId: string,
  onFrame: (event: TerminalEvent) => void,
  onReset: (missed: MissedFrames) => void,
): Promise<() => void> {
  if (mode !== 'remote' || !remote) throw 'gateway_disconnected: no gateway is connected';
  return remote.watchTerminal(terminalId, onFrame, onReset);
}
/**
 * A hole in the live stream: frames `fromSeq`..`toSeq` were dropped by the gateway's
 * ring. The payload names the range and nothing else, so the handler decides what to
 * refetch (`lagged` carries no session). It follows that a frame the client lost may
 * belong to any task, which is why recovering from one reloads more than the open one.
 */
export async function subscribe(name: 'lagged', callback: (event: MissedFrames) => void): Promise<() => void>;
export async function subscribe(name: 'agent-update-progress', callback: (event: import('./agent-updates').AgentUpdateProgress) => void): Promise<() => void>;
export async function subscribe(name: 'jev-run', callback: (event: import('./jev').JevReport) => void): Promise<() => void>;
export async function subscribe(name: 'jev-frame', callback: (event: import('./jev-live').JevFrame) => void): Promise<() => void>;
export async function subscribe(name: 'jev-install', callback: (event: { status: string; message: string }) => void): Promise<() => void>;
export async function subscribe(name: 'sessions-changed', callback: () => void): Promise<() => void>;
export async function subscribe(name: 'queue-changed', callback: (event: { sessionId: string }) => void): Promise<() => void>;
export async function subscribe(name: 'bots-changed', callback: () => void): Promise<() => void>;
export async function subscribe(name: 'bot-notification', callback: (event: { botId: string; text: string }) => void): Promise<() => void>;
export async function subscribe(name: 'bot-pairing', callback: (event: { botId: string; channelId: string; senderName: string; code: string }) => void): Promise<() => void>;
export async function subscribe(name: 'sessions-deleted', callback: (event: {sessionIds: string[]}) => void): Promise<() => void>;
export async function subscribe(name: 'provider-models-changed', callback: (event: {provider: import('./types').ProviderId}) => void): Promise<() => void>;
export async function subscribe(name: 'overlay-open-task', callback: (event: { sessionId: string; workerId?: string; workerSessionId?: string; subagentId?: string }) => void): Promise<() => void>;
export async function subscribe(name: 'architect-event', callback: (event: import('./architect-types').TeamEvent) => void): Promise<() => void>;
export async function subscribe(name: 'agent-ui-action', callback: (event: import('./agent-ui-actions').AgentUiAction) => void): Promise<() => void>;
export async function subscribe(name: 'android-setup', callback: (event: import('./simulator').AndroidSetup) => void): Promise<() => void>;
export async function subscribe(name: 'cloud-setup', callback: (event: { provider: string; label: string }) => void): Promise<() => void>;
export async function subscribe(name: 'vm-changed', callback: (event: { show?: boolean } | string[] | null) => void): Promise<() => void>;
export async function subscribe(name: 'simulator-changed', callback: (event: { udid: string; show: boolean }) => void): Promise<() => void>;
export async function subscribe(name: 'agent-event', callback: (event: AgentEvent) => void): Promise<() => void>;
export async function subscribe(name: 'terminal-event', callback: (event: TerminalEvent) => void): Promise<() => void>;
export async function subscribe(name: 'browser-state', callback: (event: BrowserStatus) => void): Promise<() => void>;
export async function subscribe(name: 'browser-element', callback: (event: BrowserElement) => void): Promise<() => void>;
export async function subscribe(name: string, callback: (event: any) => void): Promise<() => void> {
  const sub: Subscription = { name, callback };
  subscriptions.add(sub);
  await attach(sub);
  return async () => {
    subscriptions.delete(sub);
    const dispose = disposers.get(sub);
    disposers.delete(sub);
    await dispose?.();
  };
}
export async function chooseFolder(title = 'Open a project in PandaCode'): Promise<string | null> {
  // A local dialog would return a path on this machine, which does not exist on the
  // gateway; the remote folder picker takes over from here.
  if (mode === 'remote') return null;
  if (mode === 'preview') return '/Users/you/Projects/new-project';
  const result = await (await import('@tauri-apps/plugin-dialog')).open({ directory: true, multiple: false, title });
  return typeof result === 'string' ? result : null;
}
/** Files to attach to a message. The preview returns a fixed sample selection. */
export async function chooseAttachments(): Promise<string[]> {
  if (mode === 'remote') return [];
  if (mode === 'preview') return (await import('./preview')).samplePick('files');
  const result = await (await import('@tauri-apps/plugin-dialog')).open({ multiple: true, title: 'Attach to your message', filters: [
    { name: 'Images and documents', extensions: ['png', 'jpg', 'jpeg', 'webp', 'pdf', 'txt', 'md', 'markdown', 'csv', 'json', 'yaml', 'yml', 'xml', 'html', 'rtf', 'doc', 'docx', 'odt', 'pages', 'key', 'numbers', 'pptx', 'xlsx', 'epub', 'log'] },
    { name: 'All files', extensions: ['*'] },
  ] });
  return Array.isArray(result) ? result : typeof result === 'string' ? [result] : [];
}
/** A VS Code extension package holding color themes. */
export async function chooseVsix(): Promise<string | null> {
  if (mode === 'remote') return null;
  if (mode === 'preview') return '/Users/you/Downloads/tokyo-night.vsix';
  const result = await (await import('@tauri-apps/plugin-dialog')).open({ multiple: false, title: 'Import a VS Code theme', filters: [{ name: 'VS Code extension', extensions: ['vsix'] }] });
  return typeof result === 'string' ? result : null;
}
export async function chooseAttachmentFolder(): Promise<string | null> {
  if (mode === 'remote') return null;
  if (mode === 'preview') return (await import('./preview')).samplePick('folder')[0] ?? null;
  const result = await (await import('@tauri-apps/plugin-dialog')).open({ directory: true, multiple: false, title: 'Attach a folder to your message' });
  return typeof result === 'string' ? result : null;
}
